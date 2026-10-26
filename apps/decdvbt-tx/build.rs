// made by CasualArclamp
//! Puts the icon (and the name and version Explorer shows in the file's
//! properties) into the Windows exe. Other targets have nothing to do. If the
//! resource compiler (`rc.exe`, from the Windows SDK) is not there the exe is
//! built without an icon and a warning says so: the window still has its own.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set("ProductName", "DecDVB-T TX");
    res.set("FileDescription", "DecDVB-T DVB-T transmitter");
    res.set("LegalCopyright", "GPL-3.0-or-later");
    if let Err(e) = res.compile() {
        println!("cargo:warning=the icon was not put in the exe: {e}");
    }
}
