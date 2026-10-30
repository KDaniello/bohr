// Makes the calls a conformance case asks for, in C#.
//
// A driver knows how to call its own SDK and nothing else: the case says what to call, the checker
// says whether the records were right, and this file is the only language-specific part.
//
// The case is parsed with the framework's own JSON reader. That is not a dependency — it ships
// with the runtime — and the SDK under test still writes its records by hand, which is where the
// no-dependency rule actually bites.

using System.Globalization;
using System.Text.Json;

namespace Bohr.Conformance;

internal static class Driver
{
    private static int Main(string[] args)
    {
        if (args.Length < 1)
        {
            Console.Error.WriteLine("usage: conformance-driver <case json>");
            return 2;
        }

        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(args[0]);
        }
        catch (JsonException e)
        {
            Console.Error.WriteLine($"the case does not parse: {e.Message}");
            return 2;
        }

        if (!document.RootElement.TryGetProperty("calls", out var calls)
            || calls.ValueKind != JsonValueKind.Array)
        {
            Console.Error.WriteLine("the case names no calls");
            return 2;
        }

        foreach (var call in calls.EnumerateArray())
        {
            var name = call.TryGetProperty("fn", out var fn) ? fn.GetString() ?? string.Empty : string.Empty;
            var arguments = call.TryGetProperty("args", out var raw) && raw.ValueKind == JsonValueKind.Array
                ? raw.EnumerateArray().ToArray()
                : Array.Empty<JsonElement>();

            var flag = arguments.Length > 0 && arguments[0].ValueKind == JsonValueKind.True;
            string Text(int i) =>
                arguments.Length > i && arguments[i].ValueKind == JsonValueKind.String
                    ? arguments[i].GetString() ?? string.Empty
                    : string.Empty;
            string Json(int i) =>
                arguments.Length > i ? arguments[i].GetRawText() : "null";

            switch (name)
            {
                case "always":
                    Sdk.Always(flag, Text(1));
                    break;
                case "always_or_unreachable":
                    Sdk.AlwaysOrUnreachable(flag, Text(1));
                    break;
                case "sometimes":
                    Sdk.Sometimes(flag, Text(1));
                    break;
                case "reachable":
                    Sdk.Reachable(Text(0));
                    break;
                case "unreachable":
                    Sdk.Unreachable(Text(0));
                    break;
                case "fault_point":
                    _ = Sdk.FaultPoint(Text(0));
                    break;
                case "setup_complete":
                    Sdk.SetupComplete(Json(0));
                    break;
                case "send_event":
                    Sdk.SendEvent(Text(0), Json(1));
                    break;
                case "random_bytes":
                    var n = arguments.Length > 0 && arguments[0].ValueKind == JsonValueKind.Number
                        ? arguments[0].GetInt32()
                        : 0;
                    _ = Sdk.RandomBytes(n);
                    break;
                default:
                    Console.Error.WriteLine(
                        string.Format(CultureInfo.InvariantCulture, "this driver has no call named {0}", name));
                    return 2;
            }
        }

        return 0;
    }
}
