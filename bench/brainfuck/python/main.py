import sys

def parse(source):
    ops, _ = parse_body(source, 0)
    return ops

def parse_body(source, pos):
    acc = []
    length = len(source)
    while pos < length:
        c = source[pos]
        if c == '+' or c == '-':
            val = 1 if c == '+' else -1
            pos += 1
            while pos < length and (source[pos] == '+' or source[pos] == '-'):
                val += 1 if source[pos] == '+' else -1
                pos += 1
            if val != 0:
                acc.append((1, val)) # 1: Add
        elif c == '>' or c == '<':
            val = 1 if c == '>' else -1
            pos += 1
            while pos < length and (source[pos] == '>' or source[pos] == '<'):
                val += 1 if source[pos] == '>' else -1
                pos += 1
            if val != 0:
                acc.append((2, val)) # 2: Move
        elif c == '.':
            acc.append((3, 0)) # 3: Out
            pos += 1
        elif c == ',':
            acc.append((4, 0)) # 4: In
            pos += 1
        elif c == '[':
            body, next_pos = parse_body(source, pos + 1)
            acc.append((5, body)) # 5: Loop
            pos = next_pos
        elif c == ']':
            return acc, pos + 1
        else:
            pos += 1
    return acc, pos

def execute(ops, tape, ptr):
    for op_type, val in ops:
        if op_type == 1: # Add
            tape[ptr] += val
        elif op_type == 2: # Move
            ptr += val
        elif op_type == 3: # Out
            sys.stdout.write(str(tape[ptr]))
        elif op_type == 4: # In
            pass
        elif op_type == 5: # Loop
            while tape[ptr] != 0:
                ptr = execute(val, tape, ptr)
    return ptr

def main():
    program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]"
    ops = parse(program)
    tape = [0] * 30000
    execute(ops, tape, 0)
    print(f"Result: {tape[8]}")

if __name__ == "__main__":
    main()
