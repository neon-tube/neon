def parse(source)
  ops, _ = parse_body(source, 0)
  ops
end

def parse_body(source, pos)
  acc = []
  len = source.length
  while pos < len
    c = source[pos]
    if c == '+' || c == '-'
      val = (c == '+') ? 1 : -1
      pos += 1
      while pos < len && (source[pos] == '+' || source[pos] == '-')
        val += (source[pos] == '+') ? 1 : -1
        pos += 1
      end
      acc << [:add, val] if val != 0
    elsif c == '>' || c == '<'
      val = (c == '>') ? 1 : -1
      pos += 1
      while pos < len && (source[pos] == '>' || source[pos] == '<')
        val += (source[pos] == '>') ? 1 : -1
        pos += 1
      end
      acc << [:move, val] if val != 0
    elsif c == '.'
      acc << [:out, 0]
      pos += 1
    elsif c == ','
      acc << [:in, 0]
      pos += 1
    elsif c == '['
      body, next_pos = parse_body(source, pos + 1)
      acc << [:loop, body]
      pos = next_pos
    elsif c == ']'
      return acc, pos + 1
    else
      pos += 1
    end
  end
  return acc, pos
end

def execute(ops, tape, ptr)
  i = 0
  limit = ops.length
  while i < limit
    op = ops[i]
    kind = op[0]
    val = op[1]
    
    if kind == :add
      tape[ptr] += val
    elsif kind == :move
      ptr += val
    elsif kind == :out
      print tape[ptr]
    elsif kind == :in
      # no-op
    elsif kind == :loop
      while tape[ptr] != 0
        ptr = execute(val, tape, ptr)
      end
    end
    i += 1
  end
  return ptr
end

def main
  program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]"
  ops = parse(program)
  tape = Array.new(30000, 0)
  execute(ops, tape, 0)
  puts "Result: #{tape[8]}"
end

main
