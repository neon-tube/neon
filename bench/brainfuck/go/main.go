package main

import (
	"fmt"
)

type OpKind int

const (
	OpAdd OpKind = iota
	OpMove
	OpOut
	OpIn
	OpLoop
)

type Op struct {
	Kind  OpKind
	Value int64
	Body  []Op
}

func parse(source string) []Op {
	ops, _ := parseBody(source, 0)
	return ops
}

func parseBody(source string, pos int) ([]Op, int) {
	var acc []Op
	for pos < len(source) {
		c := source[pos]
		if c == '+' || c == '-' {
			var val int64 = 1
			if c == '-' {
				val = -1
			}
			pos++
			for pos < len(source) && (source[pos] == '+' || source[pos] == '-') {
				if source[pos] == '+' {
					val++
				} else {
					val--
				}
				pos++
			}
			if val != 0 {
				acc = append(acc, Op{Kind: OpAdd, Value: val})
			}
		} else if c == '>' || c == '<' {
			var val int64 = 1
			if c == '<' {
				val = -1
			}
			pos++
			for pos < len(source) && (source[pos] == '>' || source[pos] == '<') {
				if source[pos] == '>' {
					val++
				} else {
					val--
				}
				pos++
			}
			if val != 0 {
				acc = append(acc, Op{Kind: OpMove, Value: val})
			}
		} else if c == '.' {
			acc = append(acc, Op{Kind: OpOut})
			pos++
		} else if c == ',' {
			acc = append(acc, Op{Kind: OpIn})
			pos++
		} else if c == '[' {
			body, nextPos := parseBody(source, pos+1)
			acc = append(acc, Op{Kind: OpLoop, Body: body})
			pos = nextPos
		} else if c == ']' {
			return acc, pos+1
		} else {
			pos++
		}
	}
	return acc, pos
}

func execute(ops []Op, tape []int64, ptr int) int {
	for i := 0; i < len(ops); i++ {
		op := ops[i]
		switch op.Kind {
		case OpAdd:
			tape[ptr] += op.Value
		case OpMove:
			ptr += int(op.Value)
		case OpOut:
			fmt.Printf("%d", tape[ptr])
		case OpIn:
		case OpLoop:
			for tape[ptr] != 0 {
				ptr = execute(op.Body, tape, ptr)
			}
		}
	}
	return ptr
}

func main() {
	program := "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]"
	ops := parse(program)
	tape := make([]int64, 30000)
	execute(ops, tape, 0)
	fmt.Printf("Result: %d\n", tape[8])
}
