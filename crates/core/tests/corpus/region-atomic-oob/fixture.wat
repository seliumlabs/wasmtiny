(module
    (memory 2 2 shared)
    (func (export "probe") (param i32) (result i32)
        local.get 0
        i32.const 65536
        i32.add
        i32.const 1
        memory.atomic.notify))
