use glib::{gstr, GStr, Value};
use gstreamer::{
	tags::{merge_strings_with_comma, CustomTag},
	Tag, TagFlag,
};

pub struct MbArtistId;

impl<'a> Tag<'a> for MbArtistId {
	type TagType = &'a str;
	const TAG_NAME: &'static GStr = gstr!("musicbrainz-artistid");
}

impl CustomTag<'_> for MbArtistId {
	const FLAG: TagFlag = TagFlag::Meta;
	const NICK: &'static GStr = gstr!("artist ID");
	const DESCRIPTION: &'static GStr = gstr!("MusicBrainz artist ID");

	fn merge_func(src: &Value) -> Value {
		merge_strings_with_comma(src)
	}
}

pub struct MbAlbumArtistId;

impl<'a> Tag<'a> for MbAlbumArtistId {
	type TagType = &'a str;
	const TAG_NAME: &'static GStr = gstr!("musicbrainz-albumartistid");
}

impl CustomTag<'_> for MbAlbumArtistId {
	const FLAG: TagFlag = TagFlag::Meta;
	const NICK: &'static GStr = gstr!("album artist ID");
	const DESCRIPTION: &'static GStr = gstr!("MusicBrainz album artist ID");

	fn merge_func(src: &Value) -> Value {
		merge_strings_with_comma(src)
	}
}
