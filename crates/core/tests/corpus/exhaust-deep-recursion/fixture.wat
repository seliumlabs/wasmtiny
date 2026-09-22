(module
    (func $recurse
        call $recurse)
    (func (export "main")
        call $recurse))
