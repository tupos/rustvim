#[no_mangle]
pub extern "C" fn hello() {
    rust_os::hello_from_rust_os();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {}
}
