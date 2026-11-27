/*
ffmpeg 4.1.11 versions.
    libavutil      56.22.100
    libavcodec     58.35.100
*/

#[allow(clippy::unwrap_used)]
fn main() {
    println!("cargo:rerun-if-changed=src/wrapper.c");

    let libavutil = pkg_config::Config::new()
        .atleast_version("56.22.100")
        .probe("libavutil")
        .unwrap();

    let libavcodec = pkg_config::Config::new()
        .atleast_version("56.35.100")
        .probe("libavcodec")
        .unwrap();

    cc::Build::new()
        .file("src/wrapper.c")
        .includes(libavutil.include_paths)
        .includes(libavcodec.include_paths)
        .warnings_into_errors(true)
        .compile("wrapper");
}
