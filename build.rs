mod codegen;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=codegen");
    codegen::generate()
}
