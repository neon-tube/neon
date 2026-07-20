local function parse(source)
    local ops, _ = parse_body(source, 1)
    return ops
end

function parse_body(source, pos)
    local acc = {}
    local len = #source
    while pos <= len do
        local c = string.sub(source, pos, pos)
        if c == "+" or c == "-" then
            local val = (c == "+") and 1 or -1
            pos = pos + 1
            while pos <= len do
                local next_c = string.sub(source, pos, pos)
                if next_c == "+" then
                    val = val + 1
                    pos = pos + 1
                elseif next_c == "-" then
                    val = val - 1
                    pos = pos + 1
                else
                    break
                end
            end
            if val ~= 0 then
                table.insert(acc, { 1, val }) -- 1 = Add
            end
        elseif c == ">" or c == "<" then
            local val = (c == ">") and 1 or -1
            pos = pos + 1
            while pos <= len do
                local next_c = string.sub(source, pos, pos)
                if next_c == ">" then
                    val = val + 1
                    pos = pos + 1
                elseif next_c == "<" then
                    val = val - 1
                    pos = pos + 1
                else
                    break
                end
            end
            if val ~= 0 then
                table.insert(acc, { 2, val }) -- 2 = Move
            end
        elseif c == "." then
            table.insert(acc, { 3, 0 }) -- 3 = Out
            pos = pos + 1
        elseif c == "," then
            table.insert(acc, { 4, 0 }) -- 4 = In
            pos = pos + 1
        elseif c == "[" then
            local body, next_pos = parse_body(source, pos + 1)
            table.insert(acc, { 5, body }) -- 5 = Loop
            pos = next_pos
        elseif c == "]" then
            return acc, pos + 1
        else
            pos = pos + 1
        end
    end
    return acc, pos
end

local function execute(ops, tape, ptr)
    local len = #ops
    for i = 1, len do
        local op = ops[i]
        local op_type = op[1]
        local val = op[2]
        if op_type == 1 then
            tape[ptr] = tape[ptr] + val
        elseif op_type == 2 then
            ptr = ptr + val
        elseif op_type == 3 then
            io.write(tostring(tape[ptr]))
        elseif op_type == 4 then
            -- no-op
        elseif op_type == 5 then
            while tape[ptr] ~= 0 do
                ptr = execute(val, tape, ptr)
            end
        end
    end
    return ptr
end

local function main()
    local program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]"
    local ops = parse(program)
    local tape = {}
    for i = 0, 30000 do
        tape[i] = 0
    end
    execute(ops, tape, 0)
    print("Result: " .. tostring(tape[8]))
end

main()
