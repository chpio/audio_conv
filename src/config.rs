use anyhow::{Context, Error, Result};
use globset::GlobBuilder;
use regex::bytes::{Regex, RegexBuilder};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Config {
    pub from: PathBuf,
    pub to: PathBuf,
    pub matches: Vec<TranscodeMatch>,
}

#[derive(Clone, Debug)]
pub struct TranscodeMatch {
    pub regex: Regex,
    pub to: Transcode,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "codec")]
pub enum Transcode {
    #[serde(rename = "opus")]
    Opus {
        #[serde(default = "default_opus_bitrate")]
        bitrate: u16,

        #[serde(default = "bitrate_type_vbr")]
        bitrate_type: BitrateType,
    },

    #[serde(rename = "mp3")]
    Mp3 {
        #[serde(default = "default_mp3_bitrate")]
        bitrate: u16,

        #[serde(default = "bitrate_type_vbr")]
        bitrate_type: BitrateType,
    },
}

impl Transcode {
    pub fn extension(&self) -> &'static str {
        match self {
            Transcode::Opus { .. } => "opus",
            Transcode::Mp3 { .. } => "mp3",
        }
    }
}

fn default_opus_bitrate() -> u16 {
    160
}

fn bitrate_type_vbr() -> BitrateType {
    BitrateType::Vbr
}

fn default_mp3_bitrate() -> u16 {
    256
}

impl Default for Transcode {
    fn default() -> Self {
        Transcode::Opus {
            bitrate: default_opus_bitrate(),
            bitrate_type: bitrate_type_vbr(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub enum BitrateType {
    #[serde(rename = "cbr")]
    Cbr,
    #[serde(rename = "vbr")]
    Vbr,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    from: Option<PathBuf>,
    to: Option<PathBuf>,

    #[serde(default)]
    matches: Vec<TranscodeMatchFile>,
}

#[derive(Debug, Deserialize)]
struct TranscodeMatchFile {
    glob: Option<String>,
    regex: Option<String>,
    to: Transcode,
}

pub fn config() -> Result<Config> {
    use clap::Arg;

    let arg_matches = clap::App::new("audio-conv")
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

    let config_path = arg_matches.value_of_os("config");
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

    let default_regex = RegexBuilder::new("\\.(flac|wav)$")
        .case_insensitive(true)
        .build()
        .expect("failed compiling default match regex");

    let transcode_matches = config_file
        .as_ref()
        .map(|config_file| {
            config_file
                .matches
                .iter()
                .map(|m| {
                    let regex = match (&m.glob, &m.regex) {
                        (None, None) => default_regex.clone(),
                        (Some(_), Some(_)) => {
                            return Err(Error::msg(
                                "`glob` and `regex` set for matcher, there can only be one!\nhttps://www.youtube.com/watch?v=5JgAMM3ADCw",
                            ));
                        }
                        (Some(glob), None) => {
                            let glob = GlobBuilder::new(glob)
                                .case_insensitive(true)
                                .build()
                                .context("failed building glob")?;
                            Regex::new(glob.regex()).context("failed compiling regex")?
                        }
                        (None, Some(regex)) => RegexBuilder::new(regex)
                            .case_insensitive(true)
                            .build()
                            .context("failed compiling regex")?,
                    };

                    Ok(TranscodeMatch {
                        regex,
                        to: m.to.clone(),
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .filter(|matches| !matches.is_empty())
        .unwrap_or_else(|| {
            vec![TranscodeMatch {
                regex: default_regex,
                to: Transcode::default(),
            }]
        });

    Ok(Config {
        from: {
            arg_matches
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
        to: arg_matches
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
        matches: transcode_matches,
    })
}

fn load_config_file(path: &Path) -> Result<Option<ConfigFile>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::new(err)),
    };
    let config: ConfigFile =
        serde_yaml::from_reader(&mut file).context("could not parse config file")?;
    Ok(Some(config))
}
