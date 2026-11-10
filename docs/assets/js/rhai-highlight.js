// Rouge (the build-time highlighter) has no Rhai lexer, so Rhai blocks arrive
// as plain <pre><code class="language-rhai">. Colour them here with the same
// class names Rouge uses, and give them the wrapper the theme expects so they
// get the same frame and copy button as every other block.
(function () {
  var KEYWORDS =
    "import|export|as|fn|let|const|if|else|for|in|while|loop|do|until|return|throw|try|catch|switch|break|continue|private|global|this|true|false";
  var TOKEN = new RegExp(
    [
      "(\\/\\/[^\\n]*|\\/\\*[\\s\\S]*?\\*\\/)", // 1 comment
      '("(?:\\\\[\\s\\S]|[^"\\\\])*"|`[^`]*`)', // 2 string
      "\\b(" + KEYWORDS + ")\\b", // 3 keyword
      "\\b(\\d[\\d_]*(?:\\.\\d+)?)\\b", // 4 number
      "\\b([A-Za-z_]\\w*)(?=\\s*\\()", // 5 call
    ].join("|"),
    "g"
  );
  var CLASSES = [null, "c1", "s2", "k", "mi", "nf"];

  function paint(code) {
    var text = code.textContent;
    var out = document.createDocumentFragment();
    var last = 0;
    var m;
    TOKEN.lastIndex = 0;
    while ((m = TOKEN.exec(text))) {
      if (m.index > last) out.appendChild(document.createTextNode(text.slice(last, m.index)));
      var span = document.createElement("span");
      for (var i = 1; i < CLASSES.length; i++) if (m[i] !== undefined) span.className = CLASSES[i];
      span.textContent = m[0];
      out.appendChild(span);
      last = m.index + m[0].length;
    }
    if (last < text.length) out.appendChild(document.createTextNode(text.slice(last)));
    code.textContent = "";
    code.appendChild(out);
  }

  var blocks = document.querySelectorAll("pre > code.language-rhai");
  for (var b = 0; b < blocks.length; b++) {
    var code = blocks[b];
    var pre = code.parentNode;
    paint(code);
    var outer = document.createElement("div");
    outer.className = "language-rhai highlighter-rouge";
    var inner = document.createElement("div");
    inner.className = "highlight";
    pre.parentNode.insertBefore(outer, pre);
    pre.className = "highlight";
    inner.appendChild(pre);
    outer.appendChild(inner);
  }
})();
