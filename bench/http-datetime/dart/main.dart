// http-datetime server: answers any request with the current UTC time in ISO 8601.
import 'dart:io';

Future<void> main() async {
  final portEnv = Platform.environment['PORT'];
  final port = int.parse((portEnv == null || portEnv.isEmpty) ? '18080' : portEnv);

  final server = await HttpServer.bind(InternetAddress.loopbackIPv4, port);
  // Keep-alive is on by default for HTTP/1.1 in dart:io.
  await for (final HttpRequest request in server) {
    _handle(request);
  }
}

Future<void> _handle(HttpRequest request) async {
  try {
    // Drain the request body so the connection stays reusable.
    await request.drain<void>();
    final now = _iso(DateTime.now().toUtc());
    request.response
      ..statusCode = HttpStatus.ok
      ..headers.contentType = ContentType('text', 'plain')
      ..write(now);
    await request.response.close();
  } catch (_) {
    // Never let one bad request take the server down.
    try {
      await request.response.close();
    } catch (_) {}
  }
}

// Trim sub-second precision so the body is exactly YYYY-MM-DDTHH:MM:SSZ.
String _iso(DateTime utc) {
  final s = utc.toIso8601String();
  final dot = s.indexOf('.');
  return dot == -1 ? s : '${s.substring(0, dot)}Z';
}
