class TreeNode {
  TreeNode? left, right;
  TreeNode(this.left, this.right);
}

TreeNode make(int depth) {
  if (depth == 0) {
    return TreeNode(null, null);
  }
  return TreeNode(make(depth - 1), make(depth - 1));
}

int check(TreeNode? n) {
  if (n == null) return 0;
  return 1 + check(n.left) + check(n.right);
}

void main() {
  const maxDepth = 18;
  int total = 0;

  var stretch = make(maxDepth + 1);
  var sc = check(stretch);
  stretch = null;
  print("stretch tree of depth ${maxDepth + 1} check: $sc");
  total += sc;

  var longLived = make(maxDepth);

  for (var depth = 4; depth <= maxDepth; depth += 2) {
    var iterations = 1 << (maxDepth - depth + 4);
    var sum = 0;
    for (var i = 0; i < iterations; i++) {
      var t = make(depth);
      sum += check(t);
    }
    print("$iterations trees of depth $depth check: $sum");
    total += sum;
  }

  var ll = check(longLived);
  print("long lived tree of depth $maxDepth check: $ll");
  total += ll;

  print("Result: $total");
}
