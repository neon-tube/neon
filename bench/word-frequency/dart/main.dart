void main() {
  var counts = <String, int>{};

  int x = 42;
  int n = 10000000;

  for (int i = 0; i < n; i++) {
    x = (x * 48271) % 2147483647;
    String w = "w${x % 10000}";
    counts[w] = (counts[w] ?? 0) + 1;
  }

  int max = 0;
  int distinct = 0;
  for (var c in counts.values) {
    distinct++;
    if (c > max) max = c;
  }

  print("Result: $distinct $n $max");
}
