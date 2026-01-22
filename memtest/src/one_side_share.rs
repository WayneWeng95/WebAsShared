use wasmtime::*;
use anyhow::Result;

fn main() -> Result<()> {
    let mut config = Config::new();
    config.wasm_threads(true);
    let engine = Engine::new(&config)?;

    // 1. Create the Shared Memory
    let shared_mem = SharedMemory::new(&engine, MemoryType::shared(1, 1))?;

    // 2. Setup the Linker
    let mut linker = Linker::new(&engine);

    // 3. Create at least one Store first. 
    // The linker needs a 'context' to perform the definition.
    let mut store_writer = Store::new(&engine, ());

    // 4. Use 'store_writer' as the context for define
    linker.define(
        &mut store_writer, // This provides the AsContext requirement
        "env", 
        "memory", 
        Extern::SharedMemory(shared_mem.clone())
    )?;

    // Permission Asking Callback
    linker.func_wrap("env", "notify_agent_ready", || {
        println!("Agent: Sandbox has requested access.");
    })?;

    // 5. Instantiate Sandboxes
    let module = Module::from_file(&engine, "guest.wasm")?;
    
    // Instance A uses the store we already created
    let instance_writer = linker.instantiate(&mut store_writer, &module)?;

    // Instance B needs its own store for isolation
    let mut store_reader = Store::new(&engine, ());
    let instance_reader = linker.instantiate(&mut store_reader, &module)?;

    // 6. Execution
    println!("--- Writer Sandbox Running ---");
    let write_fn = instance_writer.get_typed_func::<(), ()>(&mut store_writer, "writer_logic")?;
    write_fn.call(&mut store_writer, ())?;

    println!("--- Reader Sandbox Running ---");
    let read_fn = instance_reader.get_typed_func::<(), i32>(&mut store_reader, "reader_logic")?;
    let value = read_fn.call(&mut store_reader, ())?;

    println!("Agent Verification: Value is {}", value);

    Ok(())
}
