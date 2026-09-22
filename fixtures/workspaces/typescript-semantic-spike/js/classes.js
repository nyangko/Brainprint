export class JsBase {
    run() { return "base"; }
}

export class JsChild extends JsBase {
    run() { return "child"; }
}

export class JsUnrelated {
    run() { return "unrelated"; }
}
