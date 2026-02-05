fn main() {
    println!("Hello, tee_app2!");
    let mut count = 0u64;
    for _ in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        count += 1;
        println!("tee_app2 tick #{count}");
    }
}
