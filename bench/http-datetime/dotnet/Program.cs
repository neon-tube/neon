using System;
using System.Net;
using System.Text;
using System.Threading.Tasks;

class Program
{
    static void Main()
    {
        var portEnv = Environment.GetEnvironmentVariable("PORT");
        var port = string.IsNullOrEmpty(portEnv) ? "18080" : portEnv;

        var listener = new HttpListener();
        listener.Prefixes.Add($"http://127.0.0.1:{port}/");
        listener.Start();

        while (true)
        {
            var ctx = listener.GetContext();
            // Handle each connection off the accept loop so we serve concurrently.
            _ = Task.Run(() => Handle(ctx));
        }
    }

    static void Handle(HttpListenerContext ctx)
    {
        try
        {
            var now = DateTime.UtcNow.ToString("yyyy-MM-ddTHH:mm:ssZ");
            var body = Encoding.UTF8.GetBytes(now);
            var res = ctx.Response;
            res.StatusCode = 200;
            res.ContentType = "text/plain";
            res.ContentLength64 = body.Length;
            // HttpListener keeps HTTP/1.1 connections alive by default.
            res.KeepAlive = true;
            res.OutputStream.Write(body, 0, body.Length);
            res.OutputStream.Close();
        }
        catch
        {
            // Ignore broken connections; never crash the server.
            try { ctx.Response.Abort(); } catch { }
        }
    }
}
