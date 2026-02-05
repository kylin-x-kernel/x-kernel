fn main() {
    println!("Hello, tee_app1!");
    let mut count = 0u64;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        count += 1;
        println!("tee_app1 tick #{count}");
    }
}
