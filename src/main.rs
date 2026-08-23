use env_logger::Builder;
use log::LevelFilter;
use serde::de::DeserializeOwned;
use std::{
    env, io::{stdin, stdout, Read, Write}, path::{Path, PathBuf}, str
};
use tera::Tera;

pub mod systemd;
use systemd::{activate_units, process_systemd};

pub mod utils;
use utils::{is_interactive, print_files, write_files};

pub mod formats;

pub mod quadlet;
use quadlet::{process_compose, process_quadlets, activate_quadlets};

use anyhow::{anyhow, Result};
use clap::{Parser, ValueEnum};

use crate::{formats::IniFiles, quadlet::{get_raw_quadlets, ComposeFile}, utils::ask_confirm};
use tempfile::Builder as TempFileBuilder;

#[derive(Parser, Debug)]
#[clap(name = "slate", version = "0.1.0", author = "squirreljetpack")]
pub struct Opts {
    #[clap(flatten)]
    pub file_cmd: FileCmd,
    #[clap(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Parser, Debug)]
#[clap(name = "slate")]
pub struct FileCmd {
    // if no input is given, then switch to console mode
    pub input: Option<PathBuf>,
    // todo: describe that this specifies a directory path for quadlet and systemd modes
    /// output filepath
    #[clap(short, long)]
    pub output: Option<PathBuf>,
    #[clap(short, long, value_enum)]
    pub from: Option<FromVariant>,
    #[clap(short, long, value_enum)]
    pub to: Option<ToVariant>,

    // also: #[clap(long, action = clap::ArgAction::Set, default_value_t = false)] for --tera=false
    #[clap(long, action = clap::ArgAction::SetTrue, overrides_with = "no_tera")]
    pub tera: bool,
    #[clap(long = "no-tera", action = clap::ArgAction::SetFalse, hide = true)]
    pub no_tera: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum FromVariant {
    Json,
    Yaml,
    Cbor,
    Ron,
    Toml,
    Bson,
}

impl FromVariant {
    // Deserialize into a struct
    pub fn deserialize_into<T>(&self, s: &[u8]) -> Result<T>
    where
        T: DeserializeOwned,
    {
        match self {
            FromVariant::Json => serde_json::from_slice(s).map_err(anyhow::Error::new),
            FromVariant::Yaml => serde_yaml::from_slice(s).map_err(anyhow::Error::new),
            FromVariant::Cbor => serde_cbor::from_slice(s).map_err(anyhow::Error::new),
            FromVariant::Ron => ron::de::from_bytes(s).map_err(anyhow::Error::new),
            FromVariant::Toml => {
                let s = str::from_utf8(s)?;
                toml::from_str(s).map_err(anyhow::Error::new)
            }
            FromVariant::Bson => bson::from_slice(s).map_err(anyhow::Error::new),
        }
    }

    // Run a callback on deserialized object without intermediate Box
    fn serialize<T>(&self, input: Vec<u8>, s: T)
    where
        T: Fn(&dyn erased_serde::Serialize),
    {
        match self {
            FromVariant::Json => {
                let v = serde_json::from_slice::<serde_json::Value>(&input).unwrap();
                s(&v);
            }
            FromVariant::Yaml => {
                let v = serde_yaml::from_slice::<serde_yaml::Value>(&input).unwrap();
                s(&v);
            }
            FromVariant::Cbor => {
                let v = serde_cbor::from_slice::<serde_cbor::Value>(&input).unwrap();
                s(&v);
            }
            FromVariant::Ron => {
                let v = ron::de::from_bytes::<ron::Value>(&input).unwrap();
                s(&v);
            }
            FromVariant::Toml => {
                let st = str::from_utf8(&input).unwrap();
                let v = toml::from_str::<toml::Value>(st).unwrap();
                s(&v);
            }
            FromVariant::Bson => {
                let v = bson::from_slice::<bson::Bson>(&input).unwrap();
                s(&v);
            }
        }
    }
}

impl From<FromVariant> for ToVariant {
    fn from(variant: FromVariant) -> Self {
        match variant {
            FromVariant::Json => ToVariant::Json,
            FromVariant::Yaml => ToVariant::Yaml,
            FromVariant::Cbor => ToVariant::Cbor,
            FromVariant::Ron => ToVariant::Ron,
            FromVariant::Toml => ToVariant::Toml,
            FromVariant::Bson => ToVariant::Bson,
        }
    }
}

// Get Variant from filepath
impl From<&PathBuf> for FromVariant {
    fn from(path: &PathBuf) -> Self {
        let p = path
            .extension()
            .expect("Extension not found, the type of the file could not be inferred.");
        match p.to_str().unwrap() {
            "bson" | "bs" => FromVariant::Bson,
            "cbor" | "cb" => FromVariant::Cbor,
            "json" => FromVariant::Json,
            "ron" => FromVariant::Ron,
            "toml" | "service" => FromVariant::Toml,
            "yaml" | "yml" => FromVariant::Yaml,
            _ => panic!("Type of the file could not be inferred"),
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq)]
pub enum ToVariant {
    Pickle,
    Bincode,
    Postcard,
    Flexbuffers,
    Json,
    PrettyJson,
    Yaml,
    Cbor,
    Ron,
    PrettyRon,
    Toml,
    Bson,
    Ini,
    Systemd,
    Quadlet,
}

impl ToVariant {
    fn from_path<P: AsRef<Path>>(path: P) -> Option<Self> {
        let p = path.as_ref().extension()?.to_str()?;
        match p {
            "bincode" | "bc" => Some(Self::Bincode),
            "bson" | "bs" => Some(Self::Bson),
            "cbor" | "cb" => Some(Self::Cbor),
            "yaml" | "yml" => Some(Self::Yaml),
            "flexbuffers" | "fb" => Some(Self::Flexbuffers),
            "postcard" | "pc" => Some(Self::Postcard),
            "pickle" | "pkl" => Some(Self::Pickle),
            "json" => Some(Self::Json),
            "hjson" => Some(Self::PrettyJson),
            "ron" => Some(Self::Ron),
            "hron" => Some(Self::PrettyRon),
            "toml" => Some(Self::Toml),
            "ini" => Some(Self::Ini),
            _ => None,
        }
    }

    fn to_buf(self, obj: &dyn erased_serde::Serialize) -> Vec<u8> {
        match self {
            ToVariant::Pickle => {
                serde_pickle::to_vec(&obj, serde_pickle::SerOptions::new()).unwrap()
            }
            ToVariant::Bincode => bincode::serialize(&obj).unwrap(),
            ToVariant::Postcard => postcard::to_allocvec(&obj).unwrap(),
            ToVariant::Flexbuffers => flexbuffers::to_vec(obj).unwrap(),
            ToVariant::Json => serde_json::to_vec(&obj).unwrap(),
            ToVariant::PrettyJson => serde_json::to_vec_pretty(&obj).unwrap(),
            ToVariant::Yaml => serde_yaml::to_string(&obj).unwrap().into_bytes(),
            ToVariant::Cbor => serde_cbor::to_vec(&obj).unwrap(),
            ToVariant::Ron => ron::to_string(&obj).unwrap().into_bytes(),
            ToVariant::PrettyRon => {
                let s = ron::ser::PrettyConfig::new();
                let s = ron::ser::to_string_pretty(&obj, s).unwrap();
                s.into_bytes()
            }
            ToVariant::Toml => toml::to_string(&obj).unwrap().into_bytes(),
            ToVariant::Bson => bson::to_vec(&obj).unwrap(),
            ToVariant::Ini => serde_ini::to_vec(&obj).unwrap(),
            _ => {
                panic!("Special variants have custom handling.")
            }
        }
    }
}

pub fn run(opts: Opts) -> Result<()> {
    let file_cmd = opts.file_cmd;
    let input = file_cmd.input;
    let from = file_cmd.from;
    let to = file_cmd.to;
    let output = file_cmd.output;
    let mut tera_enabled = file_cmd.tera;
    let verbose_enabled = opts.verbose > 0;

    let mut input_path: Option<PathBuf> = None;
    let from_variant: FromVariant;
    let mut input_bytes = Vec::new();

    match input {
        Some(inp_path) => {
            input_bytes = std::fs::read(&inp_path)?;
            if inp_path.extension().and_then(|e| e.to_str()) == Some("tera") {
                tera_enabled = true;
                let mut stripped = inp_path.clone();
                stripped.set_extension("");
                from_variant = from.unwrap_or_else(|| FromVariant::from(&stripped));
            } else {
                from_variant = from.unwrap_or_else(|| FromVariant::from(&inp_path));
            }
            input_path = Some(inp_path);
        }
        None => {
            stdin().lock().read_to_end(&mut input_bytes)?;
            from_variant = from.ok_or_else(|| {
                anyhow!("Input format must be specified with --from when reading from stdin")
            })?;
        }
    }

    let to_variant = to.unwrap_or_else(|| {
        output
            .as_ref()
            .and_then(ToVariant::from_path)
            .unwrap_or_else(|| from_variant.into())
    });

    if tera_enabled {
        let input_str = str::from_utf8(&input_bytes)?;
        let context = tera::Context::new();
        let rendered = Tera::one_off(input_str, &context, true)?;
        if verbose_enabled {
            println!("# Tera output");
            println!("{rendered}\n");
            println!("---\n");
        }
        input_bytes = rendered.into_bytes();
    }

    if to_variant == ToVariant::Systemd {
        let units: IniFiles = from_variant.deserialize_into(&input_bytes)?;

        if units.0.is_empty() {
            return Err(anyhow!(
                "Input for Systemd resulted in no units to process."
            ));
        }

        let processed_units = process_systemd(units)?;

        if let Some(output_dir) = output {
            let files = write_files(&processed_units.0, &output_dir, serde_ini::to_string)?;
            if is_interactive() {
                activate_units(files)?;
            }
        } else {
            print_files(&processed_units.0, serde_ini::to_string)?;
        }
    } else if to_variant == ToVariant::Quadlet {
        let file: ComposeFile = from_variant.deserialize_into(&input_bytes)?;
        let dir = input_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|p| if p.as_os_str().is_empty() { Path::new(".") } else { p });

        let file = process_compose(file, dir)?;

        let filename = if let Some(output_dir) = &output {
            output_dir.join("compose.yaml")
        } else {
            let tmp_file = TempFileBuilder::new().suffix(".yaml").tempfile()?;
            tmp_file.into_temp_path().to_path_buf()
        };

        let s = serde_yaml::to_string(&file)?;

        // todo: use pere
        if !filename.exists() || ask_confirm(
            &format!("File '{}' already exists. Overwrite?", filename.display()),
            true,
        )? {
            std::fs::write(&filename, &s)?;
        }
        
        let quadlets = get_raw_quadlets(&filename)?;
        let processed_quadlets = process_quadlets(quadlets, input_path.as_ref().and_then(|p| p.parent()))?;

        if let Some(output_dir) = output {
            let files = write_files(&processed_quadlets.0, &output_dir, serde_ini::to_string)?;
            if is_interactive() {
                std::env::set_current_dir(output_dir)?;
                activate_quadlets(files)?;
            }
        } else {
            print_files(&processed_quadlets.0, serde_ini::to_string)?;
        }
    } else if let Some(output_file) = output {
        from_variant.serialize(input_bytes, |obj| {
            let buf = to_variant.to_buf(obj);
            std::fs::write(&output_file, buf).unwrap();
        });
    } else {
        from_variant.serialize(input_bytes, |obj| {
            let buf = to_variant.to_buf(obj);
            stdout().lock().write_all(&buf).unwrap();
        })
    }

    Ok(())
}

#[allow(unused_variables)]
fn init_logger(opts: &Opts) {
    let rust_log = env::var("RUST_LOG").ok()
        .map(|val| val.to_lowercase());

    let mut builder = Builder::from_default_env();

    #[cfg(debug_assertions)]
    {
        if rust_log.is_none() {
            builder.filter_level(LevelFilter::Debug);
        }
    }
    #[cfg(not(debug_assertions))]
    {
        builder
            .format_module_path(false)
            .format_target(false)
            .format_timestamp(None);


        if rust_log.is_none() {
            {
                let log_level = match opts.verbose {
                    0 => LevelFilter::Info,
                    1 => LevelFilter::Info,
                    2 => LevelFilter::Debug,
                    _ => LevelFilter::Trace,
                };

                builder.filter(None, log_level);
            }
        }
    }

    builder.init();
}

fn main() {
    let opts = Opts::parse();

    init_logger(&opts);

    if let Err(e) = run(opts) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
