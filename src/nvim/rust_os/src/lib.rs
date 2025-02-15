pub fn hello_from_rust_os() {
    println!("hello from rust os");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {}
}
