mod pki;

fn main() {
    let pki = pki::WebTransportPki::generate().expect("generate pki");
    println!("WebTransport cert hash: {}", pki.cert_hash_hex);
}
