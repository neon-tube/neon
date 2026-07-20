import java.util.ArrayList;
import java.util.List;

public class Main {
    enum OpKind { Add, Move, Out, In, Loop }

    static class Op {
        OpKind kind;
        long value;
        List<Op> body;

        Op(OpKind kind, long value) {
            this.kind = kind;
            this.value = value;
            this.body = new ArrayList<>();
        }

        Op(OpKind kind, List<Op> body) {
            this.kind = kind;
            this.value = 0;
            this.body = body;
        }
    }

    static class ParseResult {
        List<Op> ops;
        int nextPos;

        ParseResult(List<Op> ops, int nextPos) {
            this.ops = ops;
            this.nextPos = nextPos;
        }
    }

    static List<Op> parse(String source) {
        return parseBody(source, 0).ops;
    }

    static ParseResult parseBody(String source, int pos) {
        List<Op> acc = new ArrayList<>();
        while (pos < source.length()) {
            char c = source.charAt(pos);
            if (c == '+' || c == '-') {
                long val = (c == '+') ? 1 : -1;
                pos++;
                while (pos < source.length() && (source.charAt(pos) == '+' || source.charAt(pos) == '-')) {
                    val += (source.charAt(pos) == '+') ? 1 : -1;
                    pos++;
                }
                if (val != 0) acc.add(new Op(OpKind.Add, val));
            } else if (c == '>' || c == '<') {
                long val = (c == '>') ? 1 : -1;
                pos++;
                while (pos < source.length() && (source.charAt(pos) == '>' || source.charAt(pos) == '<')) {
                    val += (source.charAt(pos) == '>') ? 1 : -1;
                    pos++;
                }
                if (val != 0) acc.add(new Op(OpKind.Move, val));
            } else if (c == '.') {
                acc.add(new Op(OpKind.Out, 0));
                pos++;
            } else if (c == ',') {
                acc.add(new Op(OpKind.In, 0));
                pos++;
            } else if (c == '[') {
                ParseResult res = parseBody(source, pos + 1);
                acc.add(new Op(OpKind.Loop, res.ops));
                pos = res.nextPos;
            } else if (c == ']') {
                return new ParseResult(acc, pos + 1);
            } else {
                pos++;
            }
        }
        return new ParseResult(acc, pos);
    }

    static class ExecutionState {
        long[] tape;
        int ptr;

        ExecutionState(long[] tape, int ptr) {
            this.tape = tape;
            this.ptr = ptr;
        }
    }

    static void execute(List<Op> ops, ExecutionState state) {
        int limit = ops.size();
        for (int i = 0; i < limit; i++) {
            Op op = ops.get(i);
            switch (op.kind) {
                case Add:
                    state.tape[state.ptr] += op.value;
                    break;
                case Move:
                    state.ptr += (int)op.value;
                    break;
                case Out:
                    System.out.print(state.tape[state.ptr]);
                    break;
                case In:
                    break;
                case Loop:
                    while (state.tape[state.ptr] != 0) {
                        execute(op.body, state);
                    }
                    break;
            }
        }
    }

    public static void main(String[] args) {
        String program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]";
        List<Op> ops = parse(program);
        long[] tape = new long[30000];
        ExecutionState state = new ExecutionState(tape, 0);
        execute(ops, state);
        System.out.println("Result: " + state.tape[8]);
    }
}
