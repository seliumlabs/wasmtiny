(module
    (table 1 1 funcref)
    (type $t (func))
    (func $f
        nop)
    (elem (i32.const 5) $f)
    (func (export "main")
        nop))
