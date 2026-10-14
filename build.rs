//! Menyematkan ikon dan info versi (`assets/app.rc`) ke exe. Hanya dipakai saat build;
//! tidak menambah dependensi runtime.

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    embed_resource::compile("assets/app.rc", embed_resource::NONE)
        .manifest_required()
        .expect("gagal menyematkan resource (butuh rc.exe dari Windows SDK)");
}
