import 'dart:io';
import 'dart:typed_data';

class Op {
  final String kind;
  final int val;
  final List<Op>? body;
  Op(this.kind, [this.val = 0, this.body]);
}

class ParseResult {
  final List<Op> ops;
  final int pos;
  ParseResult(this.ops, this.pos);
}

List<Op> parse(String source) {
  return parseBody(source, 0).ops;
}

ParseResult parseBody(String source, int pos) {
  var acc = <Op>[];
  while (pos < source.length) {
    var c = source[pos];
    if (c == '+' || c == '-') {
      int val = (c == '+') ? 1 : -1;
      pos++;
      while (pos < source.length && (source[pos] == '+' || source[pos] == '-')) {
        val += (source[pos] == '+') ? 1 : -1;
        pos++;
      }
      if (val != 0) acc.add(Op('add', val));
    } else if (c == '>' || c == '<') {
      int val = (c == '>') ? 1 : -1;
      pos++;
      while (pos < source.length && (source[pos] == '>' || source[pos] == '<')) {
        val += (source[pos] == '>') ? 1 : -1;
        pos++;
      }
      if (val != 0) acc.add(Op('move', val));
    } else if (c == '.') {
      acc.add(Op('out'));
      pos++;
    } else if (c == ',') {
      acc.add(Op('in'));
      pos++;
    } else if (c == '[') {
      var result = parseBody(source, pos + 1);
      acc.add(Op('loop', 0, result.ops));
      pos = result.pos;
    } else if (c == ']') {
      return ParseResult(acc, pos + 1);
    } else {
      pos++;
    }
  }
  return ParseResult(acc, pos);
}

class State {
  int ptr = 0;
}

void execute(List<Op> ops, Int32List tape, State state) {
  int limit = ops.length;
  for (int i = 0; i < limit; i++) {
    var op = ops[i];
    switch (op.kind) {
      case 'add':
        tape[state.ptr] += op.val;
        break;
      case 'move':
        state.ptr += op.val;
        break;
      case 'out':
        stdout.write(tape[state.ptr].toString());
        break;
      case 'in':
        break;
      case 'loop':
        while (tape[state.ptr] != 0) {
          execute(op.body!, tape, state);
        }
        break;
    }
  }
}

void main() {
  var program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]";
  var ops = parse(program);
  var tape = Int32List(30000);
  var state = State();
  execute(ops, tape, state);
  print("Result: ${tape[8]}");
}
