// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::HashSet,
    env,
    error::Error,
    fs::{self, File},
    io::Write,
    io::{self, BufRead, BufReader, BufWriter},
    path::{Path, PathBuf},
    process::Command,
    result::Result as StdResult,
};

use jwalk::WalkDir;
use rquickjs::{CatchResultExt, CaughtError, Context, Module, Runtime, WriteOptions};

const BUNDLE_JS_DIR: &str = "../bundle/js";

include!("src/bytecode.rs");

macro_rules! info {
    ($($tokens: tt)*) => {
        println!("cargo:info={}", format!($($tokens)*))
    }
}

macro_rules! rerun_if_changed {
    ($file: expr) => {
        println!("cargo:rerun-if-changed={}", $file)
    };
}

include!("src/compiler_common.rs");

fn main() -> StdResult<(), Box<dyn Error>> {
    llrt_build::set_nightly_cfg();

    rerun_if_changed!(BUNDLE_JS_DIR);
    rerun_if_changed!("Cargo.toml");

    let out_dir = env::var("OUT_DIR").unwrap();

    // #[cfg(feature = "lambda")]
    // {
    generate_sdk_client_endpoint_map(&out_dir)?;
    //}

    generate_bytecode_cache(&out_dir)?;

    Ok(())
}

fn generate_sdk_client_endpoint_map(out_dir: &str) -> StdResult<(), Box<dyn Error>> {
    let file = File::open("../sdk.cfg")?;
    let reader = BufReader::new(file);

    let sdk_client_endpoints_path = Path::new(out_dir).join("sdk_client_endpoints.rs");
    let mut sdk_client_endpoints_file = BufWriter::new(File::create(sdk_client_endpoints_path)?);

    let mut ph_map = phf_codegen::Map::<String>::new();

    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if !line.is_empty() && !line.starts_with('#') {
            let mut line_iter = line.split(',');
            let package_name = line_iter.next();
            if let Some(package_name) = package_name {
                let _client_name = line_iter.next();
                let _full_sdk = line_iter.next_back();
                let sdks_to_init = line_iter.collect::<Vec<&str>>().join(",");
                let package_name = package_name.trim_start_matches("client-");
                let package_name = package_name.into();
                if package_name == sdks_to_init {
                    ph_map.entry(package_name, r#""""#);
                } else {
                    ph_map.entry(package_name, &format!("\"{}\"", sdks_to_init));
                }
            }
        }
    }
    write!(
        &mut sdk_client_endpoints_file,
        "// @generated by build.rs\n\npub static SDK_CLIENT_ENDPOINTS: phf::Map<&'static str, &'static str> = {}",
        ph_map.build()
    )?;
    writeln!(&mut sdk_client_endpoints_file, ";")?;
    sdk_client_endpoints_file.flush()?;
    Ok(())
}

fn generate_bytecode_cache(out_dir: &str) -> StdResult<(), Box<dyn Error>> {
    let resolver = (DummyResolver,);
    let loader = (DummyLoader,);

    let rt = Runtime::new()?;
    rt.set_loader(resolver, loader);
    let ctx = Context::full(&rt)?;

    let bytecode_cache_path = Path::new(&out_dir).join("bytecode_cache.rs");
    let mut bytecode_cache_file = BufWriter::new(File::create(bytecode_cache_path)?);

    let mut ph_map = phf_codegen::Map::<String>::new();
    let mut lrt_filenames = vec![];
    let mut total_bytes: usize = 0;

    fs::write("../VERSION", env!("CARGO_PKG_VERSION")).expect("Unable to write VERSION file");

    #[cfg(feature = "lambda")]
    let test_file = PathBuf::new().join("@llrt").join("test.js");

    ctx.with(|ctx| {
        for dir_ent in WalkDir::new(BUNDLE_JS_DIR).into_iter().flatten() {
            let path = dir_ent.path();

            let path = path.strip_prefix(BUNDLE_JS_DIR)?.to_owned();

            let path_str = path.to_string_lossy().to_string();

            if path_str.starts_with("__tests__") || path.extension().unwrap_or_default() != "js" {
                continue;
            }

            #[cfg(feature = "lambda")]
            {
                if path == test_file {
                    continue;
                }
            }

            #[cfg(feature = "no-sdk")]
            {
                if path_str.starts_with("@aws-sdk")
                    || path_str.starts_with("@smithy")
                    || path_str.starts_with("llrt-chunk-sdk")
                {
                    continue;
                }
            }

            let source = fs::read_to_string(dir_ent.path())
                .unwrap_or_else(|_| panic!("Unable to load: {}", dir_ent.path().to_string_lossy()));

            let module_name = if !path_str.starts_with("llrt-") {
                path.with_extension("")
                    .to_string_lossy()
                    .replace('\\', "/")
                    .replace("@llrt/", "llrt:")
            } else {
                path.to_string_lossy().to_string().replace('\\', "/")
            };

            info!("Compiling module: {}", module_name);

            let lrt_path = PathBuf::from(&out_dir).join(path.with_extension(BYTECODE_EXT));
            let lrt_filename = lrt_path.to_string_lossy().to_string().replace('\\', "/");
            lrt_filenames.push(lrt_filename.clone());
            let bytes = {
                {
                    let module = Module::declare(ctx.clone(), module_name.clone(), source)?;
                    module.write(WriteOptions::default())
                }
            }
            .catch(&ctx)
            .map_err(|err| match err {
                CaughtError::Error(error) => error.to_string(),
                CaughtError::Exception(ex) => ex.to_string(),
                CaughtError::Value(value) => format!("{:?}", value),
            })?;

            total_bytes += bytes.len();

            fs::create_dir_all(lrt_path.parent().unwrap())?;
            if cfg!(feature = "uncompressed") {
                let uncompressed = add_bytecode_header(bytes, None);
                fs::write(&lrt_path, uncompressed)?;
            } else {
                fs::write(&lrt_path, bytes)?;
            }

            info!("Done!");

            ph_map.entry(
                module_name,
                &format!("include_bytes!(\"{}\")", &lrt_filename),
            );
        }

        StdResult::<_, Box<dyn Error>>::Ok(())
    })?;

    write!(
        &mut bytecode_cache_file,
        "// @generated by build.rs\n\npub static BYTECODE_CACHE: phf::Map<&'static str, &[u8]> = {}",
        ph_map.build()
    )?;
    writeln!(&mut bytecode_cache_file, ";")?;
    bytecode_cache_file.flush()?;

    info!(
        "\n===============================\nUncompressed bytecode size: {}\n===============================",
        human_file_size(total_bytes)
    );

    let compression_dictionary_path = Path::new(out_dir)
        .join("compression.dict")
        .to_string_lossy()
        .to_string();

    if cfg!(feature = "uncompressed") {
        generate_compression_dictionary(&compression_dictionary_path, &lrt_filenames)?;
    } else {
        total_bytes = compress_bytecode(compression_dictionary_path, lrt_filenames)?;

        info!(
            "\n===============================\nCompressed bytecode size: {}\n===============================",
            human_file_size(total_bytes)
        );
    }
    Ok(())
}

fn compress_bytecode(dictionary_path: String, source_files: Vec<String>) -> io::Result<usize> {
    generate_compression_dictionary(&dictionary_path, &source_files)?;

    let mut total_size = 0;
    let tmp_dir = env::temp_dir();

    for filename in source_files {
        info!("Compressing {}...", filename);

        let tmp_filename = tmp_dir
            .join(nanoid::nanoid!())
            .to_string_lossy()
            .to_string();

        fs::copy(&filename, &tmp_filename)?;

        let uncompressed_file_size = PathBuf::from(&filename).metadata()?.len() as u32;

        let output = Command::new("zstd")
            .args([
                "--ultra",
                "-22",
                "-f",
                "-D",
                &dictionary_path,
                &tmp_filename,
                "-o",
                &filename,
            ])
            .output()?;

        if !output.status.success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Failed to compress file",
            ));
        }

        let bytes = fs::read(&filename)?;
        let compressed = add_bytecode_header(bytes, Some(uncompressed_file_size));
        fs::write(&filename, compressed)?;

        let compressed_file_size = PathBuf::from(&filename).metadata().unwrap().len() as usize;

        total_size += compressed_file_size;
    }

    Ok(total_size)
}

fn generate_compression_dictionary(
    dictionary_path: &str,
    source_files: &[String],
) -> Result<(), io::Error> {
    info!("Generating compression dictionary...");
    let file_count = source_files.len();
    let mut dictionary_filenames = source_files.to_owned();
    let mut dictionary_file_set: HashSet<String> = HashSet::from_iter(dictionary_filenames.clone());
    let mut cmd = Command::new("zstd");
    cmd.args([
        "--train",
        "--train-fastcover=steps=60",
        "--maxdict=40K",
        "-o",
        dictionary_path,
    ]);
    if file_count < 5 {
        dictionary_file_set.retain(|file_path| {
            let metadata = fs::metadata(file_path).unwrap();
            let file_size = metadata.len();
            file_size >= 1024 // 1 kilobyte = 1024 bytes
        });
        cmd.arg("-B1K");
        dictionary_filenames = dictionary_file_set.into_iter().collect();
    }
    cmd.args(&dictionary_filenames);

    // To avoid cmd being too long to execute
    let out_dir = env::var("OUT_DIR").unwrap();
    let short_source_files: Vec<_> = source_files
        .iter()
        .map(|i| {
            Path::new(i)
                .strip_prefix(out_dir.clone())
                .unwrap()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    let mut cmd = cmd.current_dir(out_dir).args(short_source_files).spawn()?;
    let exit_status = cmd.wait()?;
    if !exit_status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "Failed to generate compression dictionary",
        ));
    };
    Ok(())
}
