// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
#[derive(Debug)]
#[cfg(windows)]
use std::os::windows::{
    io::{FromRawHandle, RawHandle},
    process::CommandExt,
};
#[cfg(unix)]
use std::os::{
    fd::FromRawFd,
    unix::process::{CommandExt, ExitStatusExt},
};

use std::{
    collections::HashMap,
    io::Result as IoResult,
    process::{Command as StdCommand, Stdio},
    sync::{Arc, RwLock},
};

use llrt_buffer::Buffer;
use llrt_context::CtxExtension;
use llrt_events::{EmitError, Emitter, EventEmitter, EventList};
use llrt_stream::{
    readable::{DefaultReadableStream, ReadableStream},
    writable::{DefaultWritableStream, WritableStream},
};
use llrt_utils::{
    module::{export_default, ModuleInfo},
    object::ObjectExt,
    result::ResultExt,
};
use rquickjs::{
    class::{Trace, Tracer},
    convert::Coerced,
    module::{Declarations, Exports, ModuleDef},
    prelude::{Func, Opt, Rest, This},
    Class, Ctx, Error, Exception, Function, IntoJs, Null, Object, Result, Value,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{
        broadcast::{channel as broadcast_channel, Receiver, Sender},
        oneshot::Receiver as OneshotReceiver,
    },
};

#[cfg(unix)]
macro_rules! generate_signal_from_str_fn {
    ($($signal:path),*) => {
        fn process_signal_from_str(signal: &str) -> Option<i32> {
            let signal = ["libc::", signal].concat();
            match signal.as_str() {
                $(stringify!($signal) => Some($signal),)*
                _ => None,
            }
        }

        fn signal_str_from_i32(signal: i32) -> Option<&'static str> {
            $(if signal == $signal {
                return Some(&stringify!($signal)[6..]);
            })*

             return None;
        }
    };
}

#[cfg(unix)]
generate_signal_from_str_fn!(
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGILL,
    libc::SIGABRT,
    libc::SIGFPE,
    libc::SIGKILL,
    libc::SIGSEGV,
    libc::SIGPIPE,
    libc::SIGALRM,
    libc::SIGTERM
);

#[allow(unused_variables)]
fn prepare_shell_args(
    shell: &str,
    windows_verbatim_arguments: &mut bool,
    cmd: String,
    command_args: Option<Vec<String>>,
) -> Vec<String> {
    let mut string_args = cmd;

    #[cfg(windows)]
    let shell_is_cmd = shell.ends_with("cmd") || shell.ends_with("cmd.exe");

    #[cfg(windows)]
    {
        if shell_is_cmd {
            *windows_verbatim_arguments = true;
            string_args.insert(0, '"');
        }
    }

    if let Some(command_args) = command_args {
        //reserve at least arg length +1
        let total_length = command_args.iter().map(|s| s.len() + 1).sum();
        string_args.reserve(total_length);
        string_args.push(' ');

        for arg in command_args.iter() {
            string_args.push_str(arg);
            string_args.push(' ');
        }
    } else {
        string_args.push(' ');
    }

    #[cfg(windows)]
    {
        if shell_is_cmd {
            string_args.push('"');
            return vec![
                String::from("/d"),
                String::from("/s"),
                String::from("/c"),
                string_args,
            ];
        }
    }

    vec!["-c".into(), string_args]
}

#[allow(dead_code)]
#[rquickjs::class]
#[derive(rquickjs::JsLifetime)]
pub struct ChildProcess<'js> {
    emitter: EventEmitter<'js>,
    args: Option<Vec<String>>,
    command: String,
    kill_signal_tx: Option<Sender<Option<i32>>>,
    pid: Option<u32>,
}

impl<'js> Trace<'js> for ChildProcess<'js> {
    fn trace<'a>(&self, tracer: Tracer<'a, 'js>) {
        self.emitter.trace(tracer);
    }
}

#[derive(Clone)]
enum StdioEnum {
    Piped,
    Ignore,
    Inherit,
    Fd(i32),
}
impl StdioEnum {
    fn to_stdio(&self) -> Stdio {
        match self {
            StdioEnum::Piped => Stdio::piped(),
            StdioEnum::Ignore => Stdio::null(),
            StdioEnum::Inherit => Stdio::inherit(),
            StdioEnum::Fd(id) => {
                #[cfg(unix)]
                unsafe {
                    Stdio::from_raw_fd(*id)
                }
                #[cfg(windows)]
                unsafe {
                    Stdio::from_raw_handle(*id as RawHandle)
                }
            },
        }
    }
}

#[rquickjs::methods]
impl<'js> ChildProcess<'js> {
    #[qjs(get)]
    fn pid(&self, ctx: Ctx<'js>) -> Result<Value<'js>> {
        self.pid.into_js(&ctx)
    }

    fn kill(&mut self, signal: Opt<Value<'js>>) -> Result<bool> {
        #[cfg(unix)]
        let signal = if let Some(signal) = signal.0 {
            println!("signnalll{:#?}",signal);
            if signal.is_number() {
                Some(signal.as_number().unwrap() as i32)
            } else if signal.is_string() {
                let signal = signal.as_string().unwrap().to_string()?;
                process_signal_from_str(&signal)
            } else {
                None
            }
        } else {
            process_signal_from_str("SIGTERM")
        };

        #[cfg(not(unix))]
        {
            _ = signal;
        }
        #[cfg(not(unix))]
        let signal = Some(9); // SIGKILL

        if let Some(kill_signal_tx) = self.kill_signal_tx.take() {
            return Ok(kill_signal_tx.send(signal).is_ok());
        }

        Ok(false)
    }
}

impl<'js> ChildProcess<'js> {
    fn new(
        ctx: Ctx<'js>,
        command: String,
        args: Option<Vec<String>>,
        child: IoResult<Child>,
    ) -> Result<Class<'js, Self>> {
        let (kill_signal_tx, kill_signal_rx) = broadcast_channel::<Option<i32>>(1);

        let instance = Self {
            emitter: EventEmitter::new(),
            command: command.clone(),
            args,
            pid: None,
            kill_signal_tx: Some(kill_signal_tx),
        };

        let stdout_instance = DefaultReadableStream::new(ctx.clone())?;
        let stderr_instance = DefaultReadableStream::new(ctx.clone())?;
        let stdin_instance = DefaultWritableStream::new(ctx.clone())?;

        let instance = Class::instance(ctx.clone(), instance)?;
        let instance2 = instance.clone();
        let instance3 = instance.clone();
        let instance4 = instance.clone();

        instance.set("stderr", stderr_instance.clone())?;
        instance.set("stdout", stdout_instance.clone())?;
        instance.set("stdin", stdin_instance.clone())?;

        match child {
            Ok(mut child) => {
                instance2.borrow_mut().pid = child.id();

                if let Some(child_stdin) = child.stdin.take() {
                    DefaultWritableStream::process(stdin_instance.clone(), &ctx, child_stdin)?;
                };

                let stdout_join_receiver =
                    create_output(&ctx, child.stdout.take(), stdout_instance.clone())?;

                let stderr_join_receiver =
                    create_output(&ctx, child.stderr.take(), stderr_instance.clone())?;

                let ctx2 = ctx.clone();
                let ctx3 = ctx.clone();

                ctx.spawn_exit(async move {
                    let spawn_proc = async move {
                        let mut exit_code = None;
                        let mut exit_signal = None;

                        wait_for_process(
                            child,
                            &ctx3,
                            kill_signal_rx,
                            &mut exit_code,
                            &mut exit_signal,
                        )
                        .await?;

                        let code = exit_code.unwrap_or_default().into_js(&ctx3)?;

                        let signal = get_signal(&ctx3, exit_signal)?;

                        ChildProcess::emit_str(
                            This(instance2.clone()),
                            &ctx3,
                            "exit",
                            vec![code.clone(), signal.clone()],
                            false,
                        )?;

                        if let Some(stderr_join_receiver) = stderr_join_receiver {
                            //ok if sender drops
                            let _ = stderr_join_receiver.await;
                        }
                        if let Some(stdout_join_receiver) = stdout_join_receiver {
                            //ok if sender drops
                            let _ = stdout_join_receiver.await;
                        }

                        WritableStream::end(This(stdin_instance));

                        ReadableStream::drain(stdout_instance, &ctx3)?;
                        ReadableStream::drain(stderr_instance, &ctx3)?;
                        // ReadableStream::read(This(stdout_instance), ctx3.clone(),None)?;
                        // ReadableStream::read(This(stderr_instance), ctx3.clone(),None)?;

                        ChildProcess::emit_str(
                            This(instance2.clone()),
                            &ctx3,
                            "close",
                            vec![code, signal],
                            false,
                        )?;

                        Ok::<_, Error>(())
                    };

                    spawn_proc
                        .await
                        .emit_error("child_process", &ctx2, instance4)?;

                    Ok(())
                })?;
            },
            Err(err) => {
                let ctx3 = ctx.clone();

                let err_message = format!("Child process failed to spawn \"{}\". {}", command, err);

                ctx.spawn_exit(async move {
                    if !instance3.borrow().emitter.has_listener_str("error") {
                        return Err(Exception::throw_message(&ctx3, &err_message));
                    }

                    let ex = Exception::from_message(ctx3.clone(), &err_message)?;
                    ChildProcess::emit_str(
                        This(instance3),
                        &ctx3,
                        "error",
                        vec![ex.into()],
                        false,
                    )?;
                    Ok(())
                })?;
            },
        }
        Ok(instance)
    }

    fn new_exec(
        ctx: Ctx<'js>,
        command: String,
        args: Option<Vec<String>>,
        child: IoResult<Child>,
        cb: Function<'js>,
    ) -> Result<Class<'js, Self>> {
        let (kill_signal_tx, kill_signal_rx) = broadcast_channel::<Option<i32>>(1);

        let instance = Self {
            emitter: EventEmitter::new(),
            command: command.clone(),
            args: args.clone(),
            pid: None,
            kill_signal_tx: Some(kill_signal_tx),
        };

        let stdout_instance = DefaultReadableStream::new(ctx.clone())?;
        let stderr_instance = DefaultReadableStream::new(ctx.clone())?;
        let stdin_instance = DefaultWritableStream::new(ctx.clone())?;

        let instance = Class::instance(ctx.clone(), instance)?;
        let instance2 = instance.clone();
        let instance3 = instance.clone();
        let instance4 = instance.clone();

        instance.set("stderr", stderr_instance.clone())?;
        instance.set("stdout", stdout_instance.clone())?;
        instance.set("stdin", stdin_instance.clone())?;

        match child {
            Ok(mut child) => {
                instance2.borrow_mut().pid = child.id();

                if let Some(child_stdin) = child.stdin.take() {
                    DefaultWritableStream::process(stdin_instance.clone(), &ctx, child_stdin)?;
                };

                // let mut stdout: Option<Value<'_>> = None;
                // let mut stderr: Option<Value<'_>> = None;
                let mut stdout:Vec<u8>= Vec::new();
                let mut stderr:Vec<u8>= Vec::new();
                let mut combined_stdout_buffer: Option<&mut Vec<u8>> = Some(&mut stdout);
                let mut combined_stderr_buffer: Option<&mut Vec<u8>> = Some(&mut stderr);
                // let stdout_join_receiver = create_std_output(&ctx, &mut child.stdout, &mut stdout);
                // let stderr_join_receiver = create_std_output(&ctx, &mut child.stderr, &mut stderr);

                let stdout_join_receiver =
                    get_output(&ctx, child.stdout.take(), stdout_instance.clone(), combined_stdout_buffer)?;

                let stderr_join_receiver =
                    get_output(&ctx, child.stderr.take(), stderr_instance.clone(), combined_stderr_buffer)?;

                let ctx2 = ctx.clone();
                let ctx3 = ctx.clone();

                ctx.clone().spawn_exit(async move {
                    let spawn_proc = async move {
                        let mut exit_code = None;
                        let mut exit_signal = None;

                        
                        println!("still waiting");
                        wait_for_process(
                            child,
                            &ctx3,
                            kill_signal_rx,
                            &mut exit_code,
                            &mut exit_signal,
                        )
                        .await?;

                        // let _ = create_std_output(&ctx, &mut child.stdout, &mut stdout).await?;
                        // let _ = create_std_output(&ctx, &mut child.stderr, &mut stderr).await?;

                        // let stdout_fut = create_std_output(&ctx, &mut child.stdout, &mut stdout);
                        // let stderr_fut = create_std_output(&ctx, &mut child.stderr, &mut stderr);
                        // let wait_fut = wait_for_process(&mut child, &ctx3, kill_signal_rx, &mut exit_code, &mut exit_signal);

                        // let (_stdout_res, _stderr_res, wait_res) = tokio::join!(stdout_fut, stderr_fut, wait_fut);
                        // wait_res?;

                        let code = exit_code.unwrap_or_default().into_js(&ctx3)?;
                        let signal = get_signal(&ctx3, exit_signal)?;

                        ChildProcess::emit_str(
                            This(instance2.clone()),
                            &ctx3,
                            "exit",
                            vec![code.clone(), signal.clone()],
                            false,
                        )?;

                        if let Some(stderr_join_receiver) = stderr_join_receiver {
                            //ok if sender drops
                            let _ = stderr_join_receiver.await;
                            // println!("stouttt{}",)
                        }
                        if let Some(stdout_join_receiver) = stdout_join_receiver {
                            //ok if sender drops
                            let _ = stdout_join_receiver.await;
                        }

                        WritableStream::end(This(stdin_instance));

                        // ReadableStream::drain(stdout_instance, &ctx3)?;
                        // ReadableStream::drain(stderr_instance, &ctx3)?;

                        // let stdout_data=ReadableStream::read_data(This(stdout_instance), ctx3.clone(),Opt(None))?;
                        // let stderr_data=ReadableStream::read_data(This(stderr_instance), ctx3.clone(),Opt(None))?;
                        // let stdout_data=ReadableStream::read_data(stdout_instance)?;
                        // let stderr_data=ReadableStream::read_data(This(stderr_instance))?;

                        // stderr_data.to_string(&ctx3,"utf8");

                        let stdout_data=ReadableStream::drain_without_emitter(stdout_instance, &ctx3);
                        let stderr_data=ReadableStream::drain_without_emitter(stderr_instance, &ctx3);

                        println!("stdoutll{:#?}",stdout_data);
                        println!("stderrll{:#?}",stderr_data);
                        // if let Some(stdout) = stdout_data {
                        //     println!("stdoutll{:#?}",stdout_data.into_js(&ctx3));
                        //     // println!("stdoutll{:#?}",stdout_data.into_value(ct3.clone()));
                        // };
                        
                        // if let Some(stderr) = stderr_data {
                        //     println!("stderrll{:#?}",stderr.into_js(&ctx3));
                        // };

                        // println!("stderrll{:#?}",stdin_data.into_js(&ctx3));

                        // let stdout_data=read(This(stdout_instance),Opt(None))?;
                        // let stdin_data=read(This(stderr_instance),Opt(None))?;

                        // if let Some(stdout) = stdout_data {

                        // };

                        // if let Some(data) = This(stdout_instance).borrow().inner().buffer.read(Opt(None)) {
                        //     return Buffer(data).into_js(&ctx);
                        // }

                        // ReadableStream::read(this, ctx, size);

                        // println!("res{:#?}",stdout_instance);

                        ChildProcess::emit_str(
                            This(instance2.clone()),
                            &ctx3,
                            "close",
                            vec![code.clone(), signal],
                            false,
                        )?;

                        // println!("stdout{:#?}",stdout.clone());
                        // println!("stderr{:#?}",stderr.clone());

                        if stdout.is_some() {
                            () = cb.call((Null.into_js(&ctx3), stdout, "".into_js(&ctx3)))?;
                            Ok::<_, Error>(())
                        } else {
                            // error: Error: Command failed: ls hello
                            //     ls: hello: No such file or directory
                            //         code: 1,
                            //         killed: false,
                            //         signal: null,
                            //         cmd: 'ls hello'
                            let arg=args.unwrap();
                            let cmd=format!("{} {}", command, arg.join(" "));
                            let err_message = format!("error: Error: Command failed: {} args{}", command, cmd);
                            let error_object = Object::new(ctx3.clone())?;
                            error_object.set("message", err_message)?;
                            error_object.set("code", code)?;
                            error_object.set("cmd", cmd)?;
                            println!("err obj{:#?}",error_object.clone().into_js(&ctx3));



                            () = cb.call((error_object.into_js(&ctx3), "".into_js(&ctx3), stderr))?;
                            Ok::<_, Error>(())
                        }
                    };

                    spawn_proc
                        .await
                        .emit_error("child_process", &ctx2, instance4)?;

                    Ok(())
                })?;
            },
            Err(err) => {
                let ctx3 = ctx.clone();
                println!("commign to err{:#?}",err);

                let err_message = format!("Child process failed to spawn \"{}\". {}", command, err);

                ctx.spawn_exit(async move {
                    let ex = Exception::from_message(ctx3.clone(), &err_message)?;
                    ChildProcess::emit_str(
                        This(instance3),
                        &ctx3,
                        "error",
                        vec![ex.clone().into()],
                        false,
                    )?;

                    () = cb.call((ex, "".into_js(&ctx3), "".into_js(&ctx3)))?;
                    Ok(())
                })?;
            },
        }
        Ok(instance)
    }
}

async fn wait_for_process(
    mut child: Child,
    ctx: &Ctx<'_>,
    mut kill_signal_rx: Receiver<Option<i32>>,
    exit_code: &mut Option<i32>,
    exit_signal: &mut Option<i32>,
) -> Result<()> {
    println!("wtttttt");
    loop {
        println!("innside loop");
        tokio::select! {
            status = child.wait() => {
                println!("waithnngg");
                let exit_status = status.or_throw(ctx)?;
                exit_code.replace(exit_status.code().unwrap_or_default());

                #[cfg(unix)]
                {
                    exit_signal.replace(exit_status.signal().unwrap_or_default());
                }
                #[cfg(not(unix))]
                {
                    _ = exit_signal;
                }
                break;
            }
            Ok(signal) = kill_signal_rx.recv() => {
                #[cfg(unix)]
                {
                    if let Some(signal) = signal {
                        println!("kil signnal recievdv{}",signal);
                        if let Some(pid) = child.id() {
                            if unsafe { libc::killpg(pid as i32, signal) } == 0 {
                                continue;
                            } else {
                               return Err(Exception::throw_message(ctx, &["Failed to send signal ",itoa::Buffer::new().format(signal)," to process ", itoa::Buffer::new().format(pid)].concat()));
                            }
                        }
                    } else {
                        println!("kil signnal else");
                        child.kill().await.or_throw(ctx)?;
                        break;
                    }
                }
                #[cfg(not(unix))]
                {
                    _ = signal;
                    child.kill().await.or_throw(ctx)?;
                    break;
                }
            },
        }
    }

    Ok(())
}

impl<'js> Emitter<'js> for ChildProcess<'js> {
    fn get_event_list(&self) -> Arc<RwLock<EventList<'js>>> {
        self.emitter.get_event_list()
    }
}

fn spawn<'js>(
    ctx: Ctx<'js>,
    cmd: String,
    args_and_opts: Rest<Value<'js>>,
) -> Result<Class<'js, ChildProcess<'js>>> {
    let args_0 = args_and_opts.first();
    let args_1 = args_and_opts.get(1);
    let mut opts = None;

    if args_1.is_some() {
        opts = args_1.and_then(|o| o.as_object()).map(|o| o.to_owned());
    }

    let mut command_args = match get_command_args(&ctx, args_0, &mut opts) {
        Ok(Some(args)) => Some(args),
        Ok(None) => None,
        Err(err) => return Err(err),
    };

    let mut windows_verbatim_arguments: bool = get_windows_verbatim_arguments(&opts)?;

    let cmd = get_cmd(
        &opts,
        &mut command_args,
        &mut windows_verbatim_arguments,
        cmd,
    )?;

    let mut command = StdCommand::new(cmd.clone());
    set_command_args(&mut command, &command_args, windows_verbatim_arguments);

    let mut stdin = StdioEnum::Piped;
    let mut stdout = StdioEnum::Piped;
    let mut stderr = StdioEnum::Piped;

    if let Some(opts) = opts {
        get_gid(&opts, &mut command)?;
        get_uid(&opts, &mut command)?;
        get_cwd(&opts, &mut command)?;
        get_env(&opts, &mut command)?;
        get_stdio(&ctx, &opts, &mut stdin, &mut stdout, &mut stderr)?;
    }

    command.stdin(stdin.to_stdio());
    command.stdout(stdout.to_stdio());
    command.stderr(stderr.to_stdio());

    #[cfg(unix)]
    {
        command.process_group(0);
    }

    //tokio command does not have all std command features stabilized
    let mut command = Command::from(command);
    ChildProcess::new(ctx.clone(), cmd, command_args, command.spawn())
}

fn exec_file<'js>(
    ctx: Ctx<'js>,
    cmd: String,
    args_and_opts: Rest<Value<'js>>,
) -> Result<Class<'js, ChildProcess<'js>>> {
    let args_0 = args_and_opts.first();
    let args_1 = args_and_opts.get(1);
    let callback = args_and_opts.get(2).or_else(|| args_and_opts.last());

    let mut opts = None;

    let cb: Function = callback
        .and_then(|v| v.clone().into_function())
        .or_throw_msg(&ctx, "Callback parameter is not a function")?;

    if let Some(arg) = &args_1 {
        if !arg.is_function() {
            opts = arg.as_object().map(|o| o.clone());
        }
    }

    let mut command_args = match get_command_args(&ctx, args_0, &mut opts) {
        Ok(Some(args)) => Some(args),
        Ok(None) => None,
        Err(err) => return Err(err),
    };

    let mut windows_verbatim_arguments: bool = get_windows_verbatim_arguments(&opts)?;
    let cmd = get_cmd(
        &opts,
        &mut command_args,
        &mut windows_verbatim_arguments,
        cmd,
    )?;

    let mut command = StdCommand::new(cmd.clone());
    set_command_args(&mut command, &command_args, windows_verbatim_arguments);

    let stdin = StdioEnum::Piped;
    let stdout = StdioEnum::Piped;
    let stderr = StdioEnum::Piped;

    if let Some(opts) = &opts {
        get_gid(&opts, &mut command)?;
        get_uid(&opts, &mut command)?;
        get_cwd(&opts, &mut command)?;
        get_env(&opts, &mut command)?;
    }

    command.stdin(stdin.to_stdio());
    command.stdout(stdout.to_stdio());
    command.stderr(stderr.to_stdio());

    #[cfg(unix)]
    {
        command.process_group(0);
    }

    //tokio command does not have all std command features stabilized
    let mut command = Command::from(command);
    ChildProcess::new_exec(
        ctx.clone(),
        cmd,
        command_args,
        command.spawn(),
        cb,
    )
}

fn get_command_args<'js>(
    ctx: &Ctx<'_>,
    args_0: Option<&Value<'js>>,
    opts: &mut Option<Object<'js>>,
) -> Result<Option<Vec<String>>> {
    let command_args = if let Some(args_0) = args_0 {
        if args_0.is_array() {
            let args = args_0.clone().into_array().or_throw(&ctx)?;
            let mut args_vec = Vec::with_capacity(args.len());
            for arg in args.iter() {
                let arg: Value = arg?;
                let arg = arg
                    .as_string()
                    .or_throw_msg(&ctx, "argument is not a string")?;
                let arg = arg.to_string()?;
                args_vec.push(arg);
            }
            Some(args_vec)
        } else if args_0.is_object() {
            *opts = args_0.as_object().map(|o| o.to_owned());
            None
        } else if args_0.is_string() {
            return Err(Exception::throw_message(
                &ctx,
                "The \"args\" argument must be of type object",
            ));
        } else {
            None
        }
    } else {
        None
    };
    Ok(command_args)
}

fn get_windows_verbatim_arguments<'js>(opts: &Option<Object<'js>>) -> Result<bool> {
    let windows_verbatim_arguments: bool = if let Some(opts) = &opts {
        opts.get_optional::<&str, bool>("windowsVerbatimArguments")?
            .unwrap_or_default()
    } else {
        false
    };
    Ok(windows_verbatim_arguments)
}

fn get_cmd<'js>(
    opts: &Option<Object<'js>>,
    command_args: &mut Option<Vec<String>>,
    windows_verbatim_arguments: &mut bool,
    cmd: String,
) -> Result<String> {
    let cmd = if let Some(opts) = opts {
        if opts
            .get_optional::<&str, bool>("shell")?
            .unwrap_or_default()
        {
            #[cfg(windows)]
            let shell = "cmd.exe".to_string();
            #[cfg(not(windows))]
            let shell = "/bin/sh".to_string();
            *command_args = Some(prepare_shell_args(
                &shell,
                windows_verbatim_arguments,
                cmd,
                command_args.take(),
            ));
            shell
        } else if let Some(shell) = opts.get_optional::<&str, String>("shell")? {
            *command_args = Some(prepare_shell_args(
                &shell,
                windows_verbatim_arguments,
                cmd,
                command_args.take(),
            ));
            shell
        } else {
            cmd
        }
    } else {
        cmd
    };
    Ok(cmd)
}

fn get_gid<'js>(opts: &Object<'js>, command: &mut std::process::Command) -> Result<()> {
    #[cfg(unix)]
    if let Some(gid) = opts.get_optional("gid")? {
        command.gid(gid);
    }
    Ok(())
}

fn get_uid<'js>(opts: &Object<'js>, command: &mut std::process::Command) -> Result<()> {
    #[cfg(unix)]
    if let Some(uid) = opts.get_optional("uid")? {
        command.gid(uid);
    }
    Ok(())
}

fn get_cwd<'js>(opts: &Object<'js>, command: &mut std::process::Command) -> Result<()> {
    if let Some(cwd) = opts.get_optional::<_, String>("cwd")? {
        command.current_dir(&cwd);
    }
    Ok(())
}

fn get_env<'js>(opts: &Object<'js>, command: &mut std::process::Command) -> Result<()> {
    if let Some(env) = opts.get_optional::<_, HashMap<String, Coerced<String>>>("env")? {
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        command.env_clear();
        command.envs(env);
    }
    Ok(())
}

fn get_stdio<'js>(
    ctx: &Ctx<'_>,
    opts: &Object<'js>,
    stdin: &mut StdioEnum,
    stdout: &mut StdioEnum,
    stderr: &mut StdioEnum,
) -> Result<()> {
    if let Some(stdio) = opts.get_optional::<_, Value<'js>>("stdio")? {
        if let Some(stdio_str) = stdio.as_string() {
            let stdio = str_to_stdio(&ctx, &stdio_str.to_string()?)?;
            *stdin = stdio.clone();
            *stdout = stdio.clone();
            *stderr = stdio;
        } else if let Some(stdio) = stdio.as_array() {
            for (i, item) in stdio.iter::<Value>().enumerate() {
                let item = item?;
                let stdio = if item.is_undefined() || item.is_null() {
                    StdioEnum::Piped
                } else if let Some(std_io_str) = item.as_string() {
                    str_to_stdio(&ctx, &std_io_str.to_string()?)?
                } else if let Some(fd) = item.as_number() {
                    StdioEnum::Fd(fd as i32)
                } else {
                    StdioEnum::Piped
                };
                match i {
                    0 => *stdin = stdio,
                    1 => *stdout = stdio,
                    2 => *stderr = stdio,
                    _ => {
                        break;
                    },
                }
            }
        }
    }
    Ok(())
}

#[allow(unused_variables)]
fn set_command_args(
    command: &mut std::process::Command,
    args: &Option<Vec<String>>,
    windows_verbatim_arguments: bool,
) {
    if let Some(args) = args {
        #[cfg(windows)]
        {
            if windows_verbatim_arguments {
                command.raw_arg(args.join(" "));
            } else {
                command.args(args);
            }
        }

        #[cfg(not(windows))]
        {
            command.args(args);
        }
    }
}

fn get_signal<'js>(ctx3: &Ctx<'js>, exit_signal: Option<i32>) -> Result<Value<'js>> {
    let signal;
    #[cfg(unix)]
    {
        if let Some(s) = exit_signal {
            signal = signal_str_from_i32(s).into_js(&ctx3)?;
        } else {
            signal = rquickjs::Undefined.into_value(ctx3.clone());
        }
    }
    #[cfg(not(unix))]
    {
        signal = "SIGKILL".into_js(&ctx3)?;
    }
    Ok(signal)
}

fn str_to_stdio(ctx: &Ctx<'_>, input: &str) -> Result<StdioEnum> {
    match input {
        "pipe" => Ok(StdioEnum::Piped),
        "ignore" => Ok(StdioEnum::Ignore),
        "inherit" => Ok(StdioEnum::Inherit),
        _ => Err(Exception::throw_type(
            ctx,
            &format!(
                "Invalid stdio \"{}\". Expected one of: pipe, ignore, inherit",
                input
            ),
        )),
    }
}

// async fn create_std_output<'js, T>(
//     ctx: &Ctx<'js>,
//     child_std: &mut Option<T>,
//     stdvar: &mut Option<Value<'js>>,
// ) -> Result<()>
// where
//     T: AsyncRead + Unpin,
// {
//     if let Some(mut child_std) = child_std.take() {
//         let mut buf = Vec::new();
//         child_std
//             .read_to_end(&mut buf)
//             .await
//             .expect("Failed to read stdout");
//         let result = String::from_utf8_lossy(&buf);
//         let data = result.into_js(&ctx)?;

//         if !result.trim().is_empty() {
//             *stdvar = Some(data);
//         }
//     }
//     Ok(())
// }

fn get_output<'js, T>(
    ctx: &Ctx<'js>,
    output: Option<T>,
    native_readable_stream: Class<'js, DefaultReadableStream<'js>>,
    combined_std_buffer: Option<&mut Vec<u8>>
) -> Result<Option<OneshotReceiver<bool>>>
where
    T: AsyncRead + Unpin + Send + 'static,
{
    if let Some(output) = output {
        let receiver = DefaultReadableStream::process(native_readable_stream, ctx, output,combined_std_buffer)?;
        return Ok(Some(receiver));
    }

    Ok(None)
    
}

fn create_output<'js, T>(
    ctx: &Ctx<'js>,
    output: Option<T>,
    native_readable_stream: Class<'js, DefaultReadableStream<'js>>,
) -> Result<Option<OneshotReceiver<bool>>>
where
    T: AsyncRead + Unpin + Send + 'static,
{
    get_output(ctx, output, native_readable_stream, None)
}

pub struct ChildProcessModule;

impl ModuleDef for ChildProcessModule {
    fn declare(declare: &Declarations) -> Result<()> {
        declare.declare("spawn")?;
        declare.declare("execFile")?;
        declare.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> Result<()> {
        ChildProcess::add_event_emitter_prototype(ctx)?;
        DefaultWritableStream::add_writable_stream_prototype(ctx)?;
        DefaultWritableStream::add_event_emitter_prototype(ctx)?;
        DefaultReadableStream::add_readable_stream_prototype(ctx)?;
        DefaultReadableStream::add_event_emitter_prototype(ctx)?;

        export_default(ctx, exports, |default| {
            default.set("spawn", Func::from(spawn))?;
            default.set("execFile", Func::from(exec_file))?;
            Ok(())
        })?;

        Ok(())
    }
}

impl From<ChildProcessModule> for ModuleInfo<ChildProcessModule> {
    fn from(val: ChildProcessModule) -> Self {
        ModuleInfo {
            name: "child_process",
            module: val,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llrt_buffer as buffer;
    use llrt_test::{test_async_with, ModuleEvaluator};
    use rquickjs::CatchResultExt;

    #[tokio::test]
    async fn test_spawn() {
        test_async_with(|ctx| {
            Box::pin(async move {
                buffer::init(&ctx).unwrap();

                ModuleEvaluator::eval_rust::<ChildProcessModule>(ctx.clone(), "child_process")
                    .await
                    .unwrap();

                let message: String = ModuleEvaluator::eval_js(
                    ctx.clone(),
                    "test",
                    r#"
                   import {spawn} from "child_process";

                    let resolve = null;
                    const deferred = new Promise(res => {
                        resolve = res;
                    });

                    spawn("echo", ["hello"]).stdout.on("data", (data) => {
                        resolve(data.toString().trim())
                    });

                    export default await deferred;

                "#,
                )
                .await
                .catch(&ctx)
                .unwrap()
                .get("default")
                .unwrap();

                assert_eq!(message, "hello");
            })
        })
        .await;
    }

    #[tokio::test]
    async fn test_exec_file() {
        test_async_with(|ctx| {
            Box::pin(async move {
                buffer::init(&ctx).unwrap();

                ModuleEvaluator::eval_rust::<ChildProcessModule>(ctx.clone(), "child_process")
                    .await
                    .unwrap();

                let message: String = ModuleEvaluator::eval_js(
                    ctx.clone(),
                    "test",
                    r#"
                   import {execFile} from "child_process";
                   let response;
                    let resp=execFile("echo", ["hello"],(error, stdout, stderr) => {
                        if (error) {
                            // console.error(`Error1111: ${error.message}`);
                        }
                        if (stdout) {
                            // console.log(`stdout1111: ${stdout}`);
                            response=stdout;
                        }
                        if (stderr) {
                            // console.error(`stderr1111: ${stderr}`);
                        }
                    });
                     
                    export default await response;


                "#,
                )
                .await
                .catch(&ctx)
                .unwrap()
                .get("default")
                .unwrap();

                assert_eq!(message, "hello");
            })
        })
        .await;
    }

    #[tokio::test]
    async fn test_spawn_shell() {
        test_async_with(|ctx| {
            Box::pin(async move {
                buffer::init(&ctx).unwrap();

                ModuleEvaluator::eval_rust::<ChildProcessModule>(ctx.clone(), "child_process")
                    .await
                    .unwrap();

                let message: String = ModuleEvaluator::eval_js(
                    ctx.clone(),
                    "test",
                    r#"
                    import {spawn} from "child_process";

                    let resolve = null;
                    const deferred = new Promise(res => {
                        resolve = res;
                    });

                    spawn("echo", ["hello"], {
                        shell: true
                    }).stdout.on("data", (data) => {
                        resolve(data.toString().trim())
                    });

                    export default await deferred;
                "#,
                )
                .await
                .catch(&ctx)
                .unwrap()
                .get("default")
                .unwrap();

                assert_eq!(message, "hello");
            })
        })
        .await;
    }
}
