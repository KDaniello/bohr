// Tell Bohr what your program promises, and let it try to break the promise.
//
//     Bohr.Sdk.Always(log.IsMonotonic, "log is monotonic");
//     Bohr.Sdk.Sometimes(leaderChanged, "a leader change happened");
//     if (Bohr.Sdk.FaultPoint("before_commit_ack")) { /* this run wants the fault here */ }
//
// Nothing here has any logic of its own: it checks whether a channel is open, serialises a record
// and writes a line. Everything that decides anything — whether an assertion is a finding, which
// fault points are armed, where randomness comes from — is the supervisor's, so this stays
// something anyone could rewrite in an afternoon from docs/API.md section 6.
//
// The framework only, and no package references. A program that links this must not have to
// resolve a dependency of ours, which is also why the JSON is written by hand: it is a hundred
// lines here and a supply chain otherwise.

using System.Diagnostics;
using System.Globalization;
using System.Text;

namespace Bohr;

/// <summary>What a guest says about itself.</summary>
public static class Sdk
{
    /// <summary>Version of the record schema this writes.</summary>
    public const int Schema = 1;

    private const string OutputVar = "BOHR_SDK_OUTPUT";
    private const string AntithesisOutputVar = "ANTITHESIS_SDK_LOCAL_OUTPUT";
    private const string NodeVar = "BOHR_SDK_NODE";
    private const string ArmedVar = "BOHR_ARMED_FAULT_POINTS";

    private static readonly object Gate = new();
    private static bool _resolved;
    private static StreamWriter? _channel;
    private static int _node;
    private static HashSet<string> _armed = new(StringComparer.Ordinal);
    private static long _seq;

    /// <summary>Claims the condition holds every time this line is reached.</summary>
    public static void Always(bool condition, string message) =>
        Assert("always", "Always", condition, message);

    /// <summary>Claims it holds whenever reached, and that never reaching it is fine.</summary>
    public static void AlwaysOrUnreachable(bool condition, string message) =>
        Assert("always_or_unreachable", "AlwaysOrUnreachable", condition, message);

    /// <summary>Claims it holds at least once across a campaign.</summary>
    public static void Sometimes(bool condition, string message) =>
        Assert("sometimes", "Sometimes", condition, message);

    /// <summary>Records that this line was reached.</summary>
    public static void Reachable(string message) =>
        Assert("reachable", "Reachable", true, message);

    /// <summary>Records that a line that should never be reached was.</summary>
    public static void Unreachable(string message) =>
        Assert("unreachable", "Unreachable", false, message);

    /// <summary>Whether this run armed the named fault point.</summary>
    public static bool FaultPoint(string name)
    {
        var armed = Open() is not null && _armed.Contains(name);
        Emit("bohr_fault_point", "{\"armed\": " + Bool(armed) + ", \"name\": " + Text(name) + "}");
        return armed;
    }

    /// <summary>Announces that the program has finished setting itself up.</summary>
    public static void SetupComplete(string detailsJson = "null") =>
        Emit("antithesis_setup", "{\"details\": " + detailsJson + ", \"status\": \"complete\"}");

    /// <summary>Puts a named event on the timeline.</summary>
    public static void SendEvent(string name, string detailsJson = "null") =>
        Emit("antithesis_event", "{" + Text(name) + ": " + detailsJson + "}");

    /// <summary>Records an operation a store's harness began: "put", "delete" or "batch"; a null
    /// value removes its key.</summary>
    public static void KvBegun(string op, string[] keys, string?[] values, ulong seq,
        [System.Runtime.CompilerServices.CallerFilePath] string file = "",
        [System.Runtime.CompilerServices.CallerLineNumber] int line = 0) =>
        KvOp("kv.begun", op, keys, values, seq, file, line);

    /// <summary>Records an operation the store acknowledged, at the line that calls this.</summary>
    public static void KvAcked(string op, string[] keys, string?[] values, ulong seq,
        [System.Runtime.CompilerServices.CallerFilePath] string file = "",
        [System.Runtime.CompilerServices.CallerLineNumber] int line = 0) =>
        KvOp("kv.acked", op, keys, values, seq, file, line);

    /// <summary>Records a key read back: its value, or null where it is absent.</summary>
    public static void KvRead(string key, string? value) =>
        SendEvent("kv.read", "{" + Text("key") + ": " + Text(key) + ", " + Text("value") + ": " +
            (value is null ? "null" : Text(value)) + "}");

    /// <summary>Records that the store answered its first question since this start.</summary>
    public static void KvReady() => SendEvent("kv.ready", "{}");

    private static void KvOp(string stage, string op, string[] keys, string?[] values, ulong seq,
        string file, int line)
    {
        var keyList = string.Join(", ", System.Linq.Enumerable.Select(keys, Text));
        var valueList = string.Join(", ",
            System.Linq.Enumerable.Select(values, v => v is null ? "null" : Text(v)));
        SendEvent(stage, "{" + Text("op") + ": " + Text(op) + ", " + Text("keys") + ": [" + keyList +
            "], " + Text("values") + ": [" + valueList + "], " + Text("seq") + ": " + seq + ", " +
            Text("at") + ": {" + Text("file") + ": " + Text(file) + ", " + Text("line") + ": " +
            line + "}}");
    }

    /// <summary>Random bytes that belong to the run rather than to the machine.</summary>
    /// <remarks>
    /// Under Bohr these come from the entropy tape by way of the world model, so a seed reproduces
    /// them. Outside it they come from the platform, and the supervisor reports a run whose
    /// randomness escaped it rather than pretending the run was reproducible.
    /// </remarks>
    public static byte[] RandomBytes(int n)
    {
        if (Open() is not null)
        {
            SendEvent("bohr_random", "{\"n\": " + Number(n) + "}");
        }
        var bytes = new byte[n];
        using var source = File.OpenRead("/dev/urandom");
        var read = 0;
        while (read < n)
        {
            var got = source.Read(bytes, read, n - read);
            if (got <= 0)
            {
                break;
            }
            read += got;
        }
        return bytes;
    }

    /// <summary>One of the choices, or the default where there is nothing to choose from.</summary>
    public static T? RandomChoice<T>(IReadOnlyList<T> choices)
    {
        if (choices.Count == 0)
        {
            return default;
        }
        var index = BitConverter.ToUInt64(RandomBytes(8), 0) % (ulong)choices.Count;
        return choices[(int)index];
    }

    private static void Assert(string kind, string display, bool condition, string message)
    {
        if (Open() is null)
        {
            return;
        }
        var body = new StringBuilder()
            .Append("{\"assert_type\": ").Append(Text(kind))
            .Append(", \"condition\": ").Append(Bool(condition))
            .Append(", \"display_type\": ").Append(Text(display))
            .Append(", \"hit\": true")
            .Append(", \"id\": ").Append(Text(message))
            .Append(", \"location\": ").Append(Where())
            .Append(", \"message\": ").Append(Text(message))
            .Append(", \"must_hit\": ").Append(Bool(!string.Equals(kind, "unreachable", StringComparison.Ordinal)))
            .Append('}')
            .ToString();
        Emit("antithesis_assert", body);
    }

    /// <summary>Where in the guest's own code the call was made.</summary>
    /// <remarks>
    /// Found by walking out of this type rather than by counting frames: a count is right until
    /// somebody adds a helper, and then every report quietly points at the SDK instead of at the
    /// program it is reporting on.
    /// </remarks>
    private static string Where()
    {
        foreach (var frame in new StackTrace(fNeedFileInfo: true).GetFrames())
        {
            var method = frame.GetMethod();
            if (method?.DeclaringType == typeof(Sdk))
            {
                continue;
            }
            return "{\"begin_column\": 0, \"begin_line\": " + Number(frame.GetFileLineNumber())
                + ", \"file\": " + Text(frame.GetFileName() ?? string.Empty)
                + ", \"function\": " + Text(method?.Name ?? string.Empty) + "}";
        }
        return "{\"begin_column\": 0, \"begin_line\": 0, \"file\": \"\", \"function\": \"\"}";
    }

    private static void Emit(string key, string body)
    {
        var channel = Open();
        if (channel is null)
        {
            return;
        }
        lock (Gate)
        {
            _seq++;
            var line = "{" + Text(key) + ": " + body + ", \"bohr\": {\"node\": " + Number(_node)
                + ", \"schema\": " + Number(Schema) + ", \"seq\": " + Number(_seq) + "}}";
            try
            {
                // A record that cannot be written is dropped rather than raised: this is a
                // bystander in someone else's program, and taking it down over a full disk would
                // be worse than the gap the supervisor will notice in the sequence numbers.
                channel.WriteLine(line);
            }
            catch (IOException)
            {
            }
        }
    }

    /// <summary>Resolves the channel once.</summary>
    /// <remarks>
    /// Both variables set is not an error: ours wins, and the supervisor records the ambiguity. An
    /// SDK that refused to run would break a program whose author did nothing wrong.
    /// </remarks>
    private static StreamWriter? Open()
    {
        if (_resolved)
        {
            return _channel;
        }
        lock (Gate)
        {
            if (_resolved)
            {
                return _channel;
            }
            _resolved = true;
            var target = Environment.GetEnvironmentVariable(OutputVar);
            if (string.IsNullOrEmpty(target))
            {
                target = Environment.GetEnvironmentVariable(AntithesisOutputVar);
            }
            if (string.IsNullOrEmpty(target))
            {
                return null;
            }
            try
            {
                _channel = new StreamWriter(target, append: true) { AutoFlush = true };
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException)
            {
                return null;
            }
            _armed = new HashSet<string>(
                (Environment.GetEnvironmentVariable(ArmedVar) ?? string.Empty)
                    .Split(',', StringSplitOptions.RemoveEmptyEntries),
                StringComparer.Ordinal);
            _ = int.TryParse(
                Environment.GetEnvironmentVariable(NodeVar),
                NumberStyles.Integer,
                CultureInfo.InvariantCulture,
                out _node);
            return _channel;
        }
    }

    private static string Bool(bool value) => value ? "true" : "false";

    /// <summary>A number, in a form that does not depend on where the program is being run.</summary>
    private static string Number(long value) => value.ToString(CultureInfo.InvariantCulture);

    /// <summary>A JSON string, escaped by hand because nothing here may take a dependency.</summary>
    private static string Text(string value)
    {
        var escaped = new StringBuilder("\"");
        foreach (var c in value)
        {
            switch (c)
            {
                case '"':
                    escaped.Append("\\\"");
                    break;
                case '\\':
                    escaped.Append("\\\\");
                    break;
                case '\n':
                    escaped.Append("\\n");
                    break;
                case '\r':
                    escaped.Append("\\r");
                    break;
                case '\t':
                    escaped.Append("\\t");
                    break;
                default:
                    if (c < ' ')
                    {
                        escaped.Append("\\u").Append(((int)c).ToString("x4", CultureInfo.InvariantCulture));
                    }
                    else
                    {
                        escaped.Append(c);
                    }
                    break;
            }
        }
        return escaped.Append('"').ToString();
    }
}
