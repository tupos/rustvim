pub mod env;
pub mod ffi;
pub mod stdpaths;

pub fn hello_from_rust_os() {
    println!("hello from rust os");
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {}
}
