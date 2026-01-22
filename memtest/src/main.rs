use wasmtime::*;
use anyhow::Result;

fn main() -> Result<()> {
    let mut config = Config::new();
    config.wasm_threads(true);
    let engine = Engine::new(&config)?;

    // 1. STANDALONE RESOURCE: Memory is created once, globally.
    let shared_mem = SharedMemory::new(&engine, MemoryType::shared(1, 1))?;

    // 2. BLUEPRINT: Linker handles functions only.
    let mut linker: Linker<()> = Linker::new(&engine);
    linker.func_wrap("env", "notify_ready", || {
        println!("Agent: Sandbox is active on the bus.");
    })?;

    let module = Module::from_file(&engine, "guest.wasm")?;

    // 3. SANDBOX A: Instantiate by combining Linker + Standalone Memory
    let mut store_a = Store::new(&engine, ());
    // We manually create the imports array to "plug in" the memory
    let imports_a = [
        linker.get_by_import(&mut store_a, &module.imports().next().unwrap()).unwrap(), // func
        shared_mem.clone().into(), // Direct injection of standalone memory
    ];
    let instance_a = Instance::new(&mut store_a, &module, &imports_a)?;

    // 4. SANDBOX B: Do the same for the second sandbox
    let mut store_b = Store::new(&engine, ());
    let imports_b = [
        linker.get_by_import(&mut store_b, &module.imports().next().unwrap()).unwrap(), 
        shared_mem.clone().into(),
    ];
    let instance_b = Instance::new(&mut store_b, &module, &imports_b)?;

    // 5. EXECUTE
    let write_fn = instance_a.get_typed_func::<(), ()>(&mut store_a, "writer_logic")?;
    write_fn.call(&mut store_a, ())?;

    let read_fn = instance_b.get_typed_func::<(), i32>(&mut store_b, "reader_logic")?;
    let val = read_fn.call(&mut store_b, ())?;

    println!("Agent: Standalone Memory Verification: {}", val);

    Ok(())
}