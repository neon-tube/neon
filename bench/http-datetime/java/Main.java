import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.time.Instant;
import java.time.format.DateTimeFormatter;
import java.time.temporal.ChronoUnit;
import java.util.concurrent.Executors;

public class Main {
    public static void main(String[] args) throws IOException {
        String portEnv = System.getenv("PORT");
        int port = (portEnv == null || portEnv.isEmpty()) ? 18080 : Integer.parseInt(portEnv);

        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", port), 0);
        server.createContext("/", Main::handle);
        // A pool of workers so concurrent connections are served in parallel.
        server.setExecutor(Executors.newFixedThreadPool(
                Math.max(4, Runtime.getRuntime().availableProcessors() * 2)));
        server.start();
    }

    private static void handle(HttpExchange ex) throws IOException {
        // Drain and close the request body so the connection can be reused (keep-alive).
        try (InputStream in = ex.getRequestBody()) {
            byte[] buf = new byte[4096];
            while (in.read(buf) != -1) {
                // discard
            }
        }
        String now = DateTimeFormatter.ISO_INSTANT.format(
                Instant.now().truncatedTo(ChronoUnit.SECONDS));
        byte[] body = now.getBytes(StandardCharsets.UTF_8);
        ex.getResponseHeaders().set("Content-Type", "text/plain");
        ex.sendResponseHeaders(200, body.length);
        try (OutputStream out = ex.getResponseBody()) {
            out.write(body);
        }
    }
}
