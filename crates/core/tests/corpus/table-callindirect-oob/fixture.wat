(module
    (table 1 1 funcref)
    (type $t (func))
    (func $f
        nop)
    (elem (i32.const 0) $f)
    (func (export "main")
        i32.const 999
        call_indirect (type $t)))
