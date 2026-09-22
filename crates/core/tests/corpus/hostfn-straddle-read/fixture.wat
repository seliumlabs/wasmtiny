(module
    (import "env" "host_abuse" (func $abuse (param i32 i32) (result i32)))
    (memory 1 1)
    (func (export "main")
        i32.const 65530
        i32.const 8
        call $abuse
        drop))
