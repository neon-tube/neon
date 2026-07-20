using System;
using System.Collections.Generic;

namespace Brainfuck
{
    enum OpKind { Add, Move, Out, In, Loop }

    struct Op
    {
        public OpKind Kind;
        public long Value;
        public List<Op> Body;
        
        public Op(OpKind kind, long value = 0)
        {
            Kind = kind;
            Value = value;
            Body = new List<Op>();
        }

        public Op(OpKind kind, List<Op> body)
        {
            Kind = kind;
            Value = 0;
            Body = body;
        }
    }

    class Program
    {
        static (List<Op> ops, int nextPos) ParseBody(string source, int pos)
        {
            var acc = new List<Op>();
            while (pos < source.Length)
            {
                char c = source[pos];
                if (c == '+' || c == '-')
                {
                    long val = (c == '+') ? 1 : -1;
                    pos++;
                    while (pos < source.Length && (source[pos] == '+' || source[pos] == '-'))
                    {
                        val += (source[pos] == '+') ? 1 : -1;
                        pos++;
                    }
                    if (val != 0) acc.Add(new Op(OpKind.Add, val));
                }
                else if (c == '>' || c == '<')
                {
                    long val = (c == '>') ? 1 : -1;
                    pos++;
                    while (pos < source.Length && (source[pos] == '>' || source[pos] == '<'))
                    {
                        val += (source[pos] == '>') ? 1 : -1;
                        pos++;
                    }
                    if (val != 0) acc.Add(new Op(OpKind.Move, val));
                }
                else if (c == '.') { acc.Add(new Op(OpKind.Out)); pos++; }
                else if (c == ',') { acc.Add(new Op(OpKind.In)); pos++; }
                else if (c == '[')
                {
                    var (body, nextPos) = ParseBody(source, pos + 1);
                    acc.Add(new Op(OpKind.Loop, body));
                    pos = nextPos;
                }
                else if (c == ']') { return (acc, pos + 1); }
                else { pos++; }
            }
            return (acc, pos);
        }

        static List<Op> Parse(string source)
        {
            return ParseBody(source, 0).ops;
        }

        static void Execute(List<Op> ops, long[] tape, ref int ptr)
        {
            int limit = ops.Count;
            for (int i = 0; i < limit; i++)
            {
                var op = ops[i];
                switch (op.Kind)
                {
                    case OpKind.Add:
                        tape[ptr] += op.Value;
                        break;
                    case OpKind.Move:
                        ptr += (int)op.Value;
                        break;
                    case OpKind.Out:
                        Console.Write(tape[ptr]);
                        break;
                    case OpKind.In:
                        break;
                    case OpKind.Loop:
                        while (tape[ptr] != 0)
                        {
                            Execute(op.Body, tape, ref ptr);
                        }
                        break;
                }
            }
        }

        static void Main(string[] args)
        {
            string program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]";
            var ops = Parse(program);
            var tape = new long[30000];
            int ptr = 0;
            Execute(ops, tape, ref ptr);
            Console.WriteLine($"Result: {tape[8]}");
        }
    }
}
