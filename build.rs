fn main() {
    tonic_build::configure()
        .compile(&["proto/echo.proto"], &["proto"])
        .unwrap();
}
