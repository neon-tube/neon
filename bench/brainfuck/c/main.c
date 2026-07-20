#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef enum {
    OP_ADD,
    OP_MOVE,
    OP_OUT,
    OP_IN,
    OP_LOOP
} OpKind;

struct Op;

typedef struct Op {
    OpKind kind;
    long long value;
    struct Op* body;
    int body_len;
} Op;

typedef struct {
    Op* data;
    int len;
    int cap;
} OpArray;

void op_array_init(OpArray* arr) {
    arr->data = NULL;
    arr->len = 0;
    arr->cap = 0;
}

void op_array_push(OpArray* arr, Op op) {
    if (arr->len >= arr->cap) {
        arr->cap = arr->cap == 0 ? 8 : arr->cap * 2;
        arr->data = realloc(arr->data, arr->cap * sizeof(Op));
    }
    arr->data[arr->len++] = op;
}

Op* parse_body(const char* source, int* pos, int* out_len) {
    OpArray acc;
    op_array_init(&acc);
    while (source[*pos] != '\0') {
        char c = source[*pos];
        if (c == '+' || c == '-') {
            long long val = (c == '+') ? 1 : -1;
            (*pos)++;
            while (source[*pos] == '+' || source[*pos] == '-') {
                val += (source[*pos] == '+') ? 1 : -1;
                (*pos)++;
            }
            if (val != 0) {
                Op op = {OP_ADD, val, NULL, 0};
                op_array_push(&acc, op);
            }
        } else if (c == '>' || c == '<') {
            long long val = (c == '>') ? 1 : -1;
            (*pos)++;
            while (source[*pos] == '>' || source[*pos] == '<') {
                val += (source[*pos] == '>') ? 1 : -1;
                (*pos)++;
            }
            if (val != 0) {
                Op op = {OP_MOVE, val, NULL, 0};
                op_array_push(&acc, op);
            }
        } else if (c == '.') {
            Op op = {OP_OUT, 0, NULL, 0};
            op_array_push(&acc, op);
            (*pos)++;
        } else if (c == ',') {
            Op op = {OP_IN, 0, NULL, 0};
            op_array_push(&acc, op);
            (*pos)++;
        } else if (c == '[') {
            (*pos)++;
            int body_len;
            Op* body = parse_body(source, pos, &body_len);
            Op op = {OP_LOOP, 0, body, body_len};
            op_array_push(&acc, op);
        } else if (c == ']') {
            (*pos)++;
            *out_len = acc.len;
            return acc.data;
        } else {
            (*pos)++;
        }
    }
    *out_len = acc.len;
    return acc.data;
}

Op* parse(const char* source, int* out_len) {
    int pos = 0;
    return parse_body(source, &pos, out_len);
}

#define TAPE_SIZE 30000

void execute(Op* ops, int len, long long* tape, int* ptr) {
    for (int i = 0; i < len; i++) {
        Op op = ops[i];
        switch (op.kind) {
            case OP_ADD:
                tape[*ptr] += op.value;
                break;
            case OP_MOVE:
                *ptr += op.value;
                break;
            case OP_OUT:
                printf("%lld", tape[*ptr]);
                break;
            case OP_IN:
                break;
            case OP_LOOP:
                while (tape[*ptr] != 0) {
                    execute(op.body, op.body_len, tape, ptr);
                }
                break;
        }
    }
}

void free_ops(Op* ops, int len) {
    if (!ops) return;
    for (int i = 0; i < len; i++) {
        if (ops[i].kind == OP_LOOP) {
            free_ops(ops[i].body, ops[i].body_len);
        }
    }
    free(ops);
}

int main() {
    const char* program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]";
    int len;
    Op* ops = parse(program, &len);
    long long tape[TAPE_SIZE] = {0};
    int ptr = 0;
    execute(ops, len, tape, &ptr);
    printf("Result: %lld\n", tape[8]);
    free_ops(ops, len);
    return 0;
}
