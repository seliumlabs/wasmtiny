(module
    (memory 0 1)
    (func (export "main")
        loop $grow
            i32.const 1
            memory.grow
            i32.const -1
            i32.eq
            if
                unreachable
            end
            br $grow
        end))
