use std::env;
use std::path::PathBuf;

use bindgen::EnumVariation;

fn main() {
    // Get the cargo out directory.
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("env variable OUT_DIR not found"));

    let headers = [
        "libavcodec/avcodec.h",
        "libavutil/opt.h",
        "libavutil/mem.h",
        "libavutil/imgutils.h",
        "libavutil/pixdesc.h",
    ];

    let lib1 = pkg_config::probe_library("libavcodec").expect("find libavcodec");
    let lib2 = pkg_config::probe_library("libavutil").expect("find libavutil");

    let mut meta_header: Vec<_> = headers
        .iter()
        .map(|h| format!("#include <{}>\n", h))
        .collect();

    meta_header.push("const int AVErrorEAgain = AVERROR(EAGAIN);\n".into());
    meta_header.push("const int AVErrorEof = AVERROR_EOF;\n".into());

    let includes = lib1
        .include_paths
        .iter()
        .chain(lib2.include_paths.iter())
        .map(|path| format!("-I{}", path.to_string_lossy()));

    println!("cargo:rerun-if-changed=src/log-to-string.c");
    cc::Build::new()
        .file("src/log-to-string.c")
        .compile("log_to_string");

    bindgen::Builder::default()
        .clang_args(includes)
        .header_contents("build.h", &meta_header.concat())
        .allowlist_item("AV.*")
        .allowlist_item("avcodec.*")
        .allowlist_item("FF_.*")
        .allowlist_item("av_opt_set")
        .allowlist_item("av_codec_.*")
        .allowlist_item("av_frame_.*")
        .allowlist_item("av_init_packet")
        .allowlist_item("av_packet_.*")
        .allowlist_item("av_buffer_.*")
        .allowlist_item("av_strerror")
        .allowlist_item("av_log_set_level")
        .allowlist_item("av_malloc")
        .allowlist_item("av_image_.*")
        .allowlist_item("av_pix_.*")
        .allowlist_item("log_to_string.*")
        .default_enum_style(EnumVariation::Rust {
            non_exhaustive: false,
        })
        .derive_default(true)
        .derive_debug(true)
        .impl_debug(true)
        .generate_comments(false)
        .generate()
        .expect("configured bindgen")
        .write_to_file(out_dir.join("libavcodec.rs"))
        .expect("could not write bindings");
}
