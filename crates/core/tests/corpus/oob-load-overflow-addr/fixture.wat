(module
    (memory 1 1)
    (func (export "main")
        i32.const 0x7FFFFFFF
        i32.load offset=0x80000001
        drop))
