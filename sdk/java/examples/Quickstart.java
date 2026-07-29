// Minimal dr-strange quickstart — run against a `drsg serve` on :7700.
//   (compile with the built classes + Jackson on the classpath; see sdk/java/README.md)
import io.github.wangyingsm.drsg.Drsg;
import java.util.List;
import java.util.Map;

public class Quickstart {
    public static void main(String[] args) throws Exception {
        Drsg db = new Drsg(); // base http://127.0.0.1:7700; token from $DRSG_TOKEN

        db.nodeCreate(Drsg.NodeCreateParams.of("startup").withKey("alice").withLabels(List.of("Person")));
        db.nodeCreate(Drsg.NodeCreateParams.of("startup").withKey("bob").withLabels(List.of("Person")));
        db.edgeCreate(Drsg.EdgeCreateParams.of("startup", "alice", "bob", "KNOWS"));
        db.nodeUpdate(Drsg.NodeUpdateParams.of("startup").withKey("alice").withSet(Map.of("age", 30)));

        Drsg.NodeRecord alice = db.nodeGet(Drsg.NodeGetParams.of("startup").withKey("alice"));
        System.out.println("alice.age = " + alice.properties().get("age"));

        Drsg.DbStats stats = db.dbStats();
        System.out.println(stats.nodes() + " nodes, " + stats.edges() + " edge(s)");
    }
}
