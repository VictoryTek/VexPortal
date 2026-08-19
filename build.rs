fn main() {
    glib_build_tools::compile_resources(
        &["data"],
        "data/io.github.vexportal.gresource.xml",
        "compiled.gresource",
    );
}
