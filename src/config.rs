use anyhow::{Context, Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Config {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigFile {
    pub from: Option<PathBuf>,
    pub to: Option<PathBuf>,
}

pub fn config() -> Result<Config> {
    use clap::Arg;

    let matches = clap::App::new("audio-conv")
        .version(clap::crate_version!())
        .about("Converts audio files")
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .required(false)
                .takes_value(true)
                .help("path to an audio-conv config file, defaults to \"audio-conv.yaml\""),
        )
        .arg(
            Arg::with_name("from")
                .short("f")
                .long("from")
                .required(false)
                .takes_value(true)
                .help("from directory path"),
        )
        .arg(
            Arg::with_name("to")
                .short("t")
                .long("to")
                .required(false)
                .takes_value(true)
                .help("to directory path"),
        )
        .get_matches();

    let current_dir = std::env::current_dir().context("could not get current directory")?;

    let config_path = matches.value_of_os("config");
    let force_load = config_path.is_some();
    let config_path = config_path
        .map(AsRef::<Path>::as_ref)
        .unwrap_or_else(|| AsRef::<Path>::as_ref("audio-conv.yaml"));
    let config_path = current_dir.join(config_path);

    let config_dir = config_path
        .parent()
        .context("could not get parent directory of the config file")?;

    let config_file = load_config_file(&config_path)
        .with_context(|| format!("failed loading config file \"{}\"", config_path.display()))?;

    if force_load && config_file.is_none() {
        return Err(Error::msg(format!(
            "could not find config file \"{}\"",
            config_path.display()
        )));
    }

    Ok(Config {
        from: {
            matches
                .value_of_os("from")
                .map(|p| current_dir.join(p))
                .or_else(|| {
                    config_file
                        .as_ref()
                        .map(|c| c.from.as_ref())
                        .flatten()
                        .map(|p| config_dir.join(p))
                })
                .ok_or_else(|| Error::msg("\"from\" not configured"))?
                .canonicalize()
                .context("could not canonicalize \"from\" path")?
        },
        to: matches
            .value_of_os("to")
            .map(|p| current_dir.join(p))
            .or_else(|| {
                config_file
                    .as_ref()
                    .map(|c| c.to.as_ref())
                    .flatten()
                    .map(|p| config_dir.join(p))
            })
            .ok_or_else(|| Error::msg("\"to\" not configured"))?
            .canonicalize()
            .context("could not canonicalize \"to\" path")?,
    })
}

fn load_config_file(path: &Path) -> Result<Option<ConfigFile>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::new(err)),
    };
    let config: ConfigFile =
        serde_yaml::from_reader(&mut file).context("could not read config file")?;
    Ok(Some(config))
}
