using Contracts;

namespace Core;

// A second implementer, so "find the implementations" has to return a
// set rather than the one obvious answer -- and so an implementation
// query that quietly returns the first match fails.
public sealed class OtherRunner : IRunner
{
    public void Run() { }
}

// Implements nothing. Its `Run` is a same-name trap for both the
// interface member and the base member.
public sealed class NotARunner
{
    public void Run() { }
}
