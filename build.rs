fn main() {
    // Bundle the Yaru icons and stylesheet into the binary.
    glib_build_tools::compile_resources(
        &["data"],
        "data/resources.gresource.xml",
        "compiled.gresource",
    );
}
