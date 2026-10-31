use mstu_sdk::{
    CommandDescriptor, Data, EnumSchema, EnumVariant, EventDescriptor, Field, Schema, Str,
    TYPE_BOOL, TYPE_BYTES, TYPE_F64, TYPE_STRING, TYPE_U32, TYPE_U64, Type, TypeKind::TypeEnum,
    TypeSchema, Value, ValueKind, slice,
};

/// What a source hands over, field for field. Three of the five a video
/// source emits: the time, whether a decoder may start here, and the frame.
pub static INPUT_SCHEMA_FIELDS: [Field; 3] = [
    Field {
        name: Str::from_static("start_time"),
        ty: &TYPE_U64,
        description: Str::from_static("Time of the frame in microseconds."),
    },
    Field {
        name: Str::from_static("is_sync"),
        ty: &TYPE_BOOL,
        description: Str::from_static(
            "Whether a decoder can start at this frame. Nothing is decoded until one says yes, \
             so a stream joined mid-flight waits for its next key frame.",
        ),
    },
    Field {
        name: Str::from_static("bytes"),
        ty: &TYPE_BYTES,
        description: Str::from_static("Encoded video frame, Annex-B start code prefixed for h26x."),
    },
];

pub static INPUT_SCHEMA: Schema = Schema {
    fields: slice!(INPUT_SCHEMA_FIELDS),
};

pub static OUTPUT_SCHEMA_FIELDS: [Field; 4] = [
    Field {
        name: Str::from_static("start_time"),
        ty: &TYPE_U64,
        description: Str::from_static("Time of the frame this image was made from."),
    },
    Field {
        name: Str::from_static("width"),
        ty: &TYPE_U32,
        description: Str::from_static("Width of the image in pixels."),
    },
    Field {
        name: Str::from_static("height"),
        ty: &TYPE_U32,
        description: Str::from_static("Height of the image in pixels."),
    },
    Field {
        name: Str::from_static("bytes"),
        ty: &TYPE_BYTES,
        description: Str::from_static("The image, as a jpeg file."),
    },
];

pub static OUTPUT_SCHEMA: Schema = Schema {
    fields: slice!(OUTPUT_SCHEMA_FIELDS),
};

pub static DECODER_ENUM_VARIANTS: [EnumVariant; 2] = [
    EnumVariant {
        name: Str::from_static("Automatic"),
        value: Value {
            kind: ValueKind::ValueString,
            data: Data {
                string_: Str::from_static("auto"),
            },
        },
    },
    EnumVariant {
        name: Str::from_static("Software"),
        value: Value {
            kind: ValueKind::ValueString,
            data: Data {
                string_: Str::from_static("software"),
            },
        },
    },
];

pub static DECODER_ENUM_SCHEMA: EnumSchema = EnumSchema {
    variants: slice!(DECODER_ENUM_VARIANTS),
};

pub static DECODER_ENUM_TYPE: Type = Type {
    kind: TypeEnum,
    schema: TypeSchema {
        enum_: &DECODER_ENUM_SCHEMA,
    },
};

pub static SETTINGS_SCHEMA_FIELDS: [Field; 9] = [
    Field {
        name: Str::from_static("width"),
        ty: &TYPE_U32,
        description: Str::from_static(
            "Width of the images in pixels. 0 takes the frame's own width, and a width larger \
             than the frame's is clamped: this never upscales.",
        ),
    },
    Field {
        name: Str::from_static("height"),
        ty: &TYPE_U32,
        description: Str::from_static("Height in pixels. 0 follows the width, keeping the shape."),
    },
    Field {
        name: Str::from_static("quality"),
        ty: &TYPE_U32,
        description: Str::from_static("Jpeg quality, 1 to 100."),
    },
    Field {
        name: Str::from_static("rate"),
        ty: &TYPE_F64,
        description: Str::from_static(
            "Images a second. 0 makes one of every frame, which is rarely what anything \
             downstream wants.",
        ),
    },
    Field {
        name: Str::from_static("keyframes_only"),
        ty: &TYPE_BOOL,
        description: Str::from_static(
            "Decodes only the frames a decoder can start at. Far cheaper, and as often as the \
             camera's key frames come.",
        ),
    },
    Field {
        name: Str::from_static("min_change"),
        ty: &TYPE_U32,
        description: Str::from_static(
            "How much of the picture must change before another image is worth making, 0 to \
             100. 0 makes one of every frame that gets through the rate.",
        ),
    },
    Field {
        name: Str::from_static("heartbeat"),
        ty: &TYPE_U32,
        description: Str::from_static(
            "Seconds after which an image goes out even if nothing changed, so a quiet stream \
             can be told from a dead one. 0 turns it off.",
        ),
    },
    Field {
        name: Str::from_static("decoder"),
        ty: &DECODER_ENUM_TYPE,
        description: Str::from_static("Which decoder to use. Only software is built so far."),
    },
    Field {
        name: Str::from_static("preview"),
        ty: &TYPE_BOOL,
        description: Str::from_static(
            "Keeps the latest image in a file for this plugin's own page to show. A few writes \
             a second of a small file; turn it off and the page shows nothing.",
        ),
    },
];

pub static SETTINGS_SCHEMA: Schema = Schema {
    fields: slice!(SETTINGS_SCHEMA_FIELDS),
};

pub static LIVE_SCHEMA_FIELDS: [Field; 10] = [
    Field {
        name: Str::from_static("state"),
        ty: &TYPE_STRING,
        description: Str::from_static("What it is doing: idle, decoding or failed."),
    },
    Field {
        name: Str::from_static("frames"),
        ty: &TYPE_U64,
        description: Str::from_static("Frames taken in since the codec was set."),
    },
    Field {
        name: Str::from_static("images"),
        ty: &TYPE_U64,
        description: Str::from_static("Images handed on."),
    },
    Field {
        name: Str::from_static("skipped"),
        ty: &TYPE_U64,
        description: Str::from_static(
            "Frames decoded but not sent on: too alike, or more often than the rate asked for.",
        ),
    },
    Field {
        name: Str::from_static("fps"),
        ty: &TYPE_U64,
        description: Str::from_static("Images a second, as they are going out now."),
    },
    Field {
        name: Str::from_static("backend"),
        ty: &TYPE_STRING,
        description: Str::from_static("Which decoder is doing the work."),
    },
    Field {
        name: Str::from_static("note"),
        ty: &TYPE_STRING,
        description: Str::from_static("Why it is in the state it is, when it is worth saying."),
    },
    Field {
        name: Str::from_static("width"),
        ty: &TYPE_U32,
        description: Str::from_static(
            "Width of the last image made, which the settings only ask for: a size larger than \
             the frame is clamped and both sides are made even.",
        ),
    },
    Field {
        name: Str::from_static("height"),
        ty: &TYPE_U32,
        description: Str::from_static("Height of the last image made."),
    },
    Field {
        name: Str::from_static("preview"),
        ty: &TYPE_STRING,
        description: Str::from_static(
            "Path of the file holding the latest image, for a page to show. Empty when \
             previews are off or nothing has been decoded.",
        ),
    },
];

pub static LIVE_SCHEMA: Schema = Schema {
    fields: slice!(LIVE_SCHEMA_FIELDS),
};

/// The codecs this decodes, named as the rest of mstu names them.
pub static CODEC_ENUM_VARIANTS: [EnumVariant; 5] = [
    codec("H264", "h264"),
    codec("H265", "h265"),
    codec("VP9", "vp9"),
    codec("AV1", "av1"),
    codec("JPEG", "jpeg"),
];

const fn codec(name: &'static str, value: &'static str) -> EnumVariant {
    EnumVariant {
        name: Str::from_static(name),
        value: Value {
            kind: ValueKind::ValueString,
            data: Data {
                string_: Str::from_static(value),
            },
        },
    }
}

pub static CODEC_ENUM_SCHEMA: EnumSchema = EnumSchema {
    variants: slice!(CODEC_ENUM_VARIANTS),
};

pub static CODEC_ENUM_TYPE: Type = Type {
    kind: TypeEnum,
    schema: TypeSchema {
        enum_: &CODEC_ENUM_SCHEMA,
    },
};

/// The same two fields as the dumper's `codec.change`, so one subscription to
/// a source's `stream.changed` serves either.
pub static CHANGE_CODEC_COMMAND_SCHEMA_FIELDS: [Field; 2] = [
    Field {
        name: Str::from_static("codec"),
        ty: &CODEC_ENUM_TYPE,
        description: Str::from_static("Codec of the frames that follow."),
    },
    Field {
        name: Str::from_static("extras"),
        ty: &TYPE_BYTES,
        description: Str::from_static(
            "Parameter sets a decoder needs before the first frame, Annex-B start code prefixed.",
        ),
    },
];

pub static CHANGE_CODEC_COMMAND_SCHEMA: Schema = Schema {
    fields: slice!(CHANGE_CODEC_COMMAND_SCHEMA_FIELDS),
};

/// The same three fields, in the same order, as a source's `file.changed`
/// and `stream.changed`: one wiring downstream serves any of them.
pub static IMAGE_CHANGED_SCHEMA_FIELDS: [Field; 3] = [
    Field {
        name: Str::from_static("from"),
        ty: &CODEC_ENUM_TYPE,
        description: Str::from_static("The codec being decoded to get these images."),
    },
    Field {
        name: Str::from_static("codec"),
        ty: &CODEC_ENUM_TYPE,
        description: Str::from_static("What the images are, which is always jpeg."),
    },
    Field {
        name: Str::from_static("extras"),
        ty: &TYPE_BYTES,
        description: Str::from_static("Empty: a jpeg carries everything a reader of it needs."),
    },
];

pub static IMAGE_CHANGED_SCHEMA: Schema = Schema {
    fields: slice!(IMAGE_CHANGED_SCHEMA_FIELDS),
};

pub static EVENTS: [EventDescriptor; 1] = [EventDescriptor {
    event_name: Str::from_static("image.changed"),
    schema: &IMAGE_CHANGED_SCHEMA,
    description: Str::from_static(
        "Emitted when a decoder starts, so whatever takes the images knows what they are.",
    ),
}];

pub static COMMANDS: [CommandDescriptor; 1] = [CommandDescriptor {
    command_name: Str::from_static("codec.change"),
    schema: &CHANGE_CODEC_COMMAND_SCHEMA,
    description: Str::from_static("Says what the frames that follow are, and starts a decoder."),
}];
