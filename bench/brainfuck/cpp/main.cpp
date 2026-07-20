#include <iostream>
#include <vector>
#include <string>
#include <utility>

enum class OpKind {
    Add, Move, Out, In, Loop
};

struct Op {
    OpKind kind;
    long long value;
    std::vector<Op> body;
};

std::pair<std::vector<Op>, int> parse_body(const std::string& source, int pos) {
    std::vector<Op> acc;
    while (pos < source.length()) {
        char c = source[pos];
        if (c == '+' || c == '-') {
            long long val = (c == '+') ? 1 : -1;
            pos++;
            while (pos < source.length() && (source[pos] == '+' || source[pos] == '-')) {
                val += (source[pos] == '+') ? 1 : -1;
                pos++;
            }
            if (val != 0) {
                acc.push_back({OpKind::Add, val, {}});
            }
        } else if (c == '>' || c == '<') {
            long long val = (c == '>') ? 1 : -1;
            pos++;
            while (pos < source.length() && (source[pos] == '>' || source[pos] == '<')) {
                val += (source[pos] == '>') ? 1 : -1;
                pos++;
            }
            if (val != 0) {
                acc.push_back({OpKind::Move, val, {}});
            }
        } else if (c == '.') {
            acc.push_back({OpKind::Out, 0, {}});
            pos++;
        } else if (c == ',') {
            acc.push_back({OpKind::In, 0, {}});
            pos++;
        } else if (c == '[') {
            auto result = parse_body(source, pos + 1);
            acc.push_back({OpKind::Loop, 0, result.first});
            pos = result.second;
        } else if (c == ']') {
            return {acc, pos + 1};
        } else {
            pos++;
        }
    }
    return {acc, pos};
}

std::vector<Op> parse(const std::string& source) {
    return parse_body(source, 0).first;
}

void execute(const std::vector<Op>& ops, std::vector<long long>& tape, int& ptr) {
    for (const auto& op : ops) {
        switch (op.kind) {
            case OpKind::Add:
                tape[ptr] += op.value;
                break;
            case OpKind::Move:
                ptr += (int)op.value;
                break;
            case OpKind::Out:
                std::cout << tape[ptr];
                break;
            case OpKind::In:
                break;
            case OpKind::Loop:
                while (tape[ptr] != 0) {
                    execute(op.body, tape, ptr);
                }
                break;
        }
    }
}

int main() {
    std::string program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]";
    auto ops = parse(program);
    std::vector<long long> tape(30000, 0);
    int ptr = 0;
    execute(ops, tape, ptr);
    std::cout << "Result: " << tape[8] << std::endl;
    return 0;
}
