```plum
use Http;
use Json;

struct Repo { name: String, stars: Int }

// A decoder is a value. Composing decoders composes their error paths,
// so a failure five levels down still names all five.
let repo (): Json.Decoder[Repo] =
    Json.map2(
        Json.field("name", Json.string()),
        Json.field("stargazers_count", Json.int()),
        |n, s| Repo { name: n, stars: s })

let main (): Unit =
    match Http.get("http://api.github.com/repos/bradcypert/plum") {
        Err(e) => println("request failed: ${e}"),
        Ok(res) => match Json.decode_string(repo(), res.body) {
            Err(e) => println("bad payload: ${e}"),
            Ok(r) => println("${r.name} has ${r.stars} stars"),
        },
    }
```
