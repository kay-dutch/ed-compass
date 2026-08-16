//! Embeds the Windows icon and version resource into the executable.
//!
//! Without this the application has the default blank icon everywhere it is
//! seen — taskbar, Alt-Tab, the Desktop shortcut, Add/Remove Programs — which
//! is the first thing that makes a download look unfinished.

fn main() {
    println!("cargo:rerun-if-changed=assets/ed-compass.ico");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/ed-compass.ico");
        res.set("ProductName", "ED Compass");
        res.set(
            "FileDescription",
            "ED Compass — Elite Dangerous signal monitor",
        );
        res.set(
            "LegalCopyright",
            "Copyright (c) 2026 A Zimin. MIT licensed.",
        );

        // Not fatal. Compiling a resource needs `rc.exe` from the Windows SDK,
        // and a contributor who only wants to run the tests should not be
        // stopped by a missing icon.
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the icon or version info: {e}");
        }
    }
}
