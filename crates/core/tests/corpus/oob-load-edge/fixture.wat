(module
    (memory 1 1)
    (func (export "main")
        i32.const 65533
        i32.load
        drop))
