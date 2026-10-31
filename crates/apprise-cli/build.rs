fn main() {
  // Packed relative relocations (DT_RELR) take about 1 MB off the release
  // binary. Only musl builds use them: musl handles them since 1.2.4, while
  // glibc refuses to run such a binary before 2.36 (Ubuntu 22.04, Debian 11
  // and RHEL 9 are older), so glibc builds keep the classic table.
  if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("musl") {
    println!("cargo:rustc-link-arg-bins=-Wl,-z,pack-relative-relocs");
  }
}
