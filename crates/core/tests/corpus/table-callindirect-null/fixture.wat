(module
    (table 1 1 funcref)
    (type $t (func))
    (func (export "main")
        i32.const 0
        call_indirect (type $t)))
