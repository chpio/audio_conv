use anyhow::{Context, Error, Result};
use globset::GlobBuilder;
use regex::bytes::{Regex, RegexBuilder};
use serde::Deserialize;
use std::{
	io::Write,
	path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct Config {
	pub from: PathBuf,
	pub to: PathBuf,
	pub matches: Vec<TranscodeMatch>,
	pub jobs: Option<usize>,
}

#[derive(Debug)]
pub struct TranscodeMatch {
	pub regexes: Vec<Regex>,
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

	#[serde(rename = "flac")]
	Flac {
		#[serde(default = "default_flac_compression")]
		compression: u8,
	},

	#[serde(rename = "mp3")]
	Mp3 {
		#[serde(default = "default_mp3_bitrate")]
		bitrate: u16,

		#[serde(default = "bitrate_type_vbr")]
		bitrate_type: BitrateType,
	},

	#[serde(rename = "copy")]
	Copy,

	#[serde(rename = "copyaudio")]
	CopyAudio,
}

impl Transcode {
	pub fn extension(&self) -> &'static str {
		match self {
			Transcode::Opus { .. } => "opus",
			Transcode::Flac { .. } => "flac",
			Transcode::Mp3 { .. } => "mp3",
			Transcode::Copy => "",
			Transcode::CopyAudio => "",
		}
	}
}

fn default_opus_bitrate() -> u16 {
	160
}

fn default_flac_compression() -> u8 {
	5
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

	#[serde(default)]
	extensions: Vec<String>,

	to: Transcode,
}

pub fn config() -> Result<Config> {
	use clap::{App, Arg, SubCommand};

	let arg_matches = App::new("audio-conv")
		.version(clap::crate_version!())
		.about("Converts audio files")
		.arg(
			Arg::with_name("config")
				.short("c")
				.long("config")
				.required(false)
				.takes_value(true)
				.help("Path to an audio-conv config file, defaults to \"audio-conv.yaml\""),
		)
		.arg(
			Arg::with_name("from")
				.short("f")
				.long("from")
				.required(false)
				.takes_value(true)
				.help("\"from\" directory path"),
		)
		.arg(
			Arg::with_name("to")
				.short("t")
				.long("to")
				.required(false)
				.takes_value(true)
				.help("\"to\" directory path"),
		)
		.arg(
			Arg::with_name("jobs")
				.short("j")
				.long("jobs")
				.required(false)
				.takes_value(true)
				.help("Allow N jobs/transcodes at once. Defaults to number of logical cores"),
		)
		.subcommand(SubCommand::with_name("init").about("writes an example config"))
		.get_matches();

	let current_dir = std::env::current_dir().context("Could not get current directory")?;

	let config_path = arg_matches.value_of_os("config");
	let force_load = config_path.is_some();
	let config_path = config_path
		.map(AsRef::<Path>::as_ref)
		.unwrap_or_else(|| AsRef::<Path>::as_ref("audio-conv.yaml"));
	let config_path = current_dir.join(config_path);

	if let Some("init") = arg_matches.subcommand_name() {
		std::fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&config_path)
			.and_then(|mut f| f.write_all(std::include_bytes!("../example.audio-conv.yaml")))
			.with_context(|| format!("Unable to write config file to {}", config_path.display()))?;

		std::process::exit(0);
	}

	let config_dir = config_path
		.parent()
		.context("Could not get parent directory of the config file")?;

	let config_file = load_config_file(&config_path)
		.with_context(|| format!("Failed loading config file {}", config_path.display()))?;

	if force_load && config_file.is_none() {
		return Err(Error::msg(format!(
			"could not find config file \"{}\"",
			config_path.display()
		)));
	}

	let default_regex = RegexBuilder::new("\\.(flac|wav)$")
		.case_insensitive(true)
		.build()
		.expect("Failed compiling default match regex");

	let transcode_matches = config_file
		.as_ref()
		.map(|config_file| {
			config_file
				.matches
				.iter()
				.map(|m| {
					let glob = m.glob.iter().map(|glob| {
						let glob = GlobBuilder::new(glob)
							.case_insensitive(true)
							.build()
							.context("Failed building glob")?;
						let regex = Regex::new(glob.regex()).context("Failed compiling regex")?;
						Ok(regex)
					});

					let regex = m.regex.iter().map(|regex| {
						let regex = RegexBuilder::new(regex)
							.case_insensitive(true)
							.build()
							.context("Failed compiling regex")?;
						Ok(regex)
					});

					let extensions = m.extensions.iter().map(|ext| {
						let mut ext = regex::escape(ext);
						ext.insert_str(0, &"\\.");
						ext.push_str("$");

						let regex = RegexBuilder::new(&ext)
							.case_insensitive(true)
							.build()
							.context("Failed compiling regex")?;
						Ok(regex)
					});

					let mut regexes = glob
						.chain(regex)
						.chain(extensions)
						.collect::<Result<Vec<_>>>()?;

					if regexes.is_empty() {
						regexes.push(default_regex.clone());
					}

					Ok(TranscodeMatch {
						regexes,
						to: m.to.clone(),
					})
				})
				.collect::<Result<Vec<_>>>()
		})
		.transpose()?
		.filter(|matches| !matches.is_empty())
		.unwrap_or_else(|| {
			vec![TranscodeMatch {
				regexes: vec![default_regex],
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
				.context("Could not canonicalize \"from\" path")?
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
			.context("Could not canonicalize \"to\" path")?,
		matches: transcode_matches,
		jobs: arg_matches
			.value_of_os("jobs")
			.map(|jobs_os_str| {
				let jobs_str = jobs_os_str.to_str().with_context(|| {
					// TODO: use `OsStr.display` when it lands
					// https://github.com/rust-lang/rust/pull/80841
					format!(
						"Could not convert \"jobs\" argument to string due to invalid characters",
					)
				})?;
				jobs_str.parse().with_context(|| {
					format!(
						"Could not parse \"jobs\" argument \"{}\" to a number",
						&jobs_str
					)
				})
			})
			.transpose()?,
	})
}

fn load_config_file(path: &Path) -> Result<Option<ConfigFile>> {
	let mut file = match std::fs::File::open(path) {
		Ok(file) => file,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(err) => return Err(Error::new(err)),
	};
	let config: ConfigFile =
		serde_yaml::from_reader(&mut file).context("Could not parse config file")?;
	Ok(Some(config))
}
