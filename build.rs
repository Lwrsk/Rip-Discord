// Sous Windows : icône et métadonnées intégrées au .exe.
fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "RIP Discord");
        res.set("ProductName", "RIP Discord");
        res.compile().expect("ressources Windows");
    }
}
