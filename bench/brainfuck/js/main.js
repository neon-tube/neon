class Op {
    constructor(kind, val = 0, body = null) {
        this.kind = kind;
        this.val = val;
        this.body = body;
    }
}

function parse(source) {
    const [ops] = parseBody(source, 0);
    return ops;
}

function parseBody(source, pos) {
    const acc = [];
    while (pos < source.length) {
        const c = source[pos];
        if (c === '+' || c === '-') {
            let val = (c === '+') ? 1 : -1;
            pos++;
            while (pos < source.length && (source[pos] === '+' || source[pos] === '-')) {
                val += (source[pos] === '+') ? 1 : -1;
                pos++;
            }
            if (val !== 0) acc.push(new Op('add', val));
        } else if (c === '>' || c === '<') {
            let val = (c === '>') ? 1 : -1;
            pos++;
            while (pos < source.length && (source[pos] === '>' || source[pos] === '<')) {
                val += (source[pos] === '>') ? 1 : -1;
                pos++;
            }
            if (val !== 0) acc.push(new Op('move', val));
        } else if (c === '.') {
            acc.push(new Op('out'));
            pos++;
        } else if (c === ',') {
            acc.push(new Op('in'));
            pos++;
        } else if (c === '[') {
            const [body, nextPos] = parseBody(source, pos + 1);
            acc.push(new Op('loop', 0, body));
            pos = nextPos;
        } else if (c === ']') {
            return [acc, pos + 1];
        } else {
            pos++;
        }
    }
    return [acc, pos];
}

function execute(ops, tape, state) {
    const limit = ops.length;
    for (let i = 0; i < limit; i++) {
        const op = ops[i];
        switch (op.kind) {
            case 'add':
                tape[state.ptr] += op.val;
                break;
            case 'move':
                state.ptr += op.val;
                break;
            case 'out':
                process.stdout.write(tape[state.ptr].toString());
                break;
            case 'in':
                break;
            case 'loop':
                while (tape[state.ptr] !== 0) {
                    execute(op.body, tape, state);
                }
                break;
        }
    }
}

function main() {
    const program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]";
    const ops = parse(program);
    const tape = new Int32Array(30000);
    const state = { ptr: 0 };
    execute(ops, tape, state);
    console.log(`Result: ${tape[8]}`);
}

main();
