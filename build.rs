use std::env;
use std::path::PathBuf;

use bindgen::EnumVariation;

fn main() {
    // Get the cargo out directory.
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("env variable OUT_DIR not found"));

    let mut headers = vec![];

    headers.push("libavcodec/avcodec.h");
    headers.push("libavutil/opt.h");

    let library = pkg_config::probe_library("libavcodec").expect("find libavcodec");

    let mut meta_header: Vec<_> = headers
        .iter()
        .map(|h| format!("#include <{}>\n", h))
        .collect();

    meta_header.push("const int AVErrorEAgain = AVERROR(EAGAIN);\n".into());
    meta_header.push("const int AVErrorEof = AVERROR_EOF;\n".into());

    let includes = library
        .include_paths
        .iter()
        .map(|path| format!("-I{}", path.to_string_lossy()));

    bindgen::Builder::default()
        .clang_args(includes)
        .header_contents("build.h", &meta_header.concat())
        .allowlist_item("AV.*")
        .allowlist_item("avcodec.*")
        .allowlist_item("FF_.*")
        .allowlist_item("av_opt_set")
        .allowlist_item("av_codec_iterate")
        .allowlist_item("av_frame_alloc")
        .allowlist_item("av_frame_free")
        .allowlist_item("av_frame_get_buffer")
        .allowlist_item("av_init_packet")
        .allowlist_item("av_packet_unref")
        .allowlist_item("av_packet_alloc")
        .allowlist_item("av_packet_free")
        .allowlist_item("av_strerror")
        .default_enum_style(EnumVariation::Rust {
            non_exhaustive: false,
        })
        .derive_default(true)
        .generate_comments(false)
        .generate()
        .expect("configured bindgen")
        .write_to_file(out_dir.join("libavcodec.rs"))
        .expect("could not write bindings");
}
