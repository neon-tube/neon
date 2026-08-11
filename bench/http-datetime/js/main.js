// http-datetime server: answers any request with the current UTC time in ISO 8601.
// Uses the node:http API via require so it runs identically under Node (CommonJS) and Bun.
const http = require("node:http");

const port = process.env.PORT || "18080";

const server = http.createServer((req, res) => {
  // Drain the request body so keep-alive connections stay reusable.
  req.resume();
  const body = new Date().toISOString().replace(/\.\d+Z$/, "Z");
  res.writeHead(200, {
    "Content-Type": "text/plain",
    "Content-Length": Buffer.byteLength(body),
  });
  res.end(body);
});

server.listen(Number(port), "127.0.0.1");
