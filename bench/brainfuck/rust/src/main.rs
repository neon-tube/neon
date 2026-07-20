enum Op {
    Add(i64),
    Move(i64),
    Out,
    In,
    Loop(Vec<Op>),
}

fn parse(source: &str) -> Vec<Op> {
    let (ops, _) = parse_body(source, 0);
    ops
}

fn parse_body(source: &str, mut pos: usize) -> (Vec<Op>, usize) {
    let mut acc = Vec::new();
    let chars: Vec<char> = source.chars().collect();
    while pos < chars.len() {
        let c = chars[pos];
        if c == '+' || c == '-' {
            let mut val = if c == '+' { 1 } else { -1 };
            pos += 1;
            while pos < chars.len() && (chars[pos] == '+' || chars[pos] == '-') {
                val += if chars[pos] == '+' { 1 } else { -1 };
                pos += 1;
            }
            if val != 0 {
                acc.push(Op::Add(val));
            }
        } else if c == '>' || c == '<' {
            let mut val = if c == '>' { 1 } else { -1 };
            pos += 1;
            while pos < chars.len() && (chars[pos] == '>' || chars[pos] == '<') {
                val += if chars[pos] == '>' { 1 } else { -1 };
                pos += 1;
            }
            if val != 0 {
                acc.push(Op::Move(val));
            }
        } else if c == '.' {
            acc.push(Op::Out);
            pos += 1;
        } else if c == ',' {
            acc.push(Op::In);
            pos += 1;
        } else if c == '[' {
            let (body, next_pos) = parse_body(source, pos + 1);
            acc.push(Op::Loop(body));
            pos = next_pos;
        } else if c == ']' {
            return (acc, pos + 1);
        } else {
            pos += 1;
        }
    }
    (acc, pos)
}

fn execute(ops: &[Op], tape: &mut [i64], ptr: &mut usize) {
    for op in ops {
        match op {
            Op::Add(val) => {
                tape[*ptr] += val;
            }
            Op::Move(val) => {
                if *val > 0 {
                    *ptr += *val as usize;
                } else {
                    *ptr -= (-*val) as usize;
                }
            }
            Op::Out => {
                print!("{}", tape[*ptr]);
            }
            Op::In => {}
            Op::Loop(body) => {
                while tape[*ptr] != 0 {
                    execute(body, tape, ptr);
                }
            }
        }
    }
}

fn main() {
    let program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]";
    let ops = parse(program);
    let mut tape = vec![0i64; 30000];
    let mut ptr = 0;
    execute(&ops, &mut tape, &mut ptr);
    println!("Result: {}", tape[8]);
}
