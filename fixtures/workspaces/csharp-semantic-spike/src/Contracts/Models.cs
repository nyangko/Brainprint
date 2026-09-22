namespace Contracts;

public sealed class Model
{
    public string Name { get; set; } = "";
    public int Count { get; set; }
}

public readonly struct Tally
{
    public Tally(int total) => Total = total;
    public int Total { get; }
}

public enum Level
{
    Low,
    High,
}

public record Snapshot(string Label, int Value);

public delegate void Notify(string message);

public sealed class Box<T>
{
    public Box(T value) => Value = value;
    public T Value { get; }
}
