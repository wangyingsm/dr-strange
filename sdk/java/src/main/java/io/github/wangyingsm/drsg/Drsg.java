// Code generated from crates/dr-strange-web/openrpc.json by the codegen package; DO NOT EDIT.
package io.github.wangyingsm.drsg;

import com.fasterxml.jackson.core.type.TypeReference;
import java.util.List;
import java.util.Map;

/** A dr-strange server client — one method per JSON-RPC method. */
public class Drsg extends Client {

    public Drsg() {
        super();
    }

    public Drsg(String baseUrl) {
        super(baseUrl);
    }

    public Drsg(String baseUrl, String token) {
        super(baseUrl, token);
    }

    public record NodeRecord(
            long id,
            String externalKey,
            List<String> labels,
            Map<String, Object> properties,
            Double score,
            String match) {
    }

    public record EdgeRecord(
            long id,
            long src,
            long dst,
            String type,
            Map<String, Object> properties,
            String match) {
    }

    public record Subgraph(
            List<NodeRecord> nodes,
            List<EdgeRecord> edges,
            long total,
            boolean truncated) {
    }

    public record FindResult(
            List<NodeRecord> nodes,
            List<EdgeRecord> edges,
            String mode,
            String note,
            Long scanned,
            Long total,
            Boolean truncated) {
    }

    public record Deleted(
            boolean deleted,
            Long id) {
    }

    public record PlaneRef(
            long id,
            String name) {
    }

    public record PlaneCard(
            long id,
            String name,
            long nodes,
            long edges,
            Map<String, Object> properties) {
    }

    public record DbStats(
            long planes,
            long nodes,
            long edges,
            long labels,
            long edgeTypes,
            long indexes,
            long commitSeq,
            boolean persistent,
            Long fileSize) {
    }

    public record PlaneCatalogParams(
            String plane) {

        public static PlaneCatalogParams of(String plane) {
            return new PlaneCatalogParams(plane);
        }
    }

    public record NodeGetParams(
            String plane,
            Long id,
            String key) {

        public static NodeGetParams of(String plane) {
            return new NodeGetParams(plane, null, null);
        }

        public NodeGetParams withId(Long id) {
            return new NodeGetParams(plane, id, key);
        }

        public NodeGetParams withKey(String key) {
            return new NodeGetParams(plane, id, key);
        }
    }

    public record PlaneNeighborsItem(
            Long node,
            Long edge) {
    }

    public record PlaneNeighborsParams(
            String plane,
            long id,
            String direction,
            String type,
            Long asOf,
            Long asOfMs) {

        public static PlaneNeighborsParams of(String plane, long id) {
            return new PlaneNeighborsParams(plane, id, null, null, null, null);
        }

        public PlaneNeighborsParams withDirection(String direction) {
            return new PlaneNeighborsParams(plane, id, direction, type, asOf, asOfMs);
        }

        public PlaneNeighborsParams withType(String type) {
            return new PlaneNeighborsParams(plane, id, direction, type, asOf, asOfMs);
        }

        public PlaneNeighborsParams withAsOf(Long asOf) {
            return new PlaneNeighborsParams(plane, id, direction, type, asOf, asOfMs);
        }

        public PlaneNeighborsParams withAsOfMs(Long asOfMs) {
            return new PlaneNeighborsParams(plane, id, direction, type, asOf, asOfMs);
        }
    }

    public record PlaneHistoryResult(
            Long oldest,
            Long latest) {
    }

    public record PlaneSearchParams(
            String plane,
            String property,
            List<Double> query,
            String label,
            Long k,
            String metric) {

        public static PlaneSearchParams of(String plane, String property, List<Double> query) {
            return new PlaneSearchParams(plane, property, query, null, null, null);
        }

        public PlaneSearchParams withLabel(String label) {
            return new PlaneSearchParams(plane, property, query, label, k, metric);
        }

        public PlaneSearchParams withK(Long k) {
            return new PlaneSearchParams(plane, property, query, label, k, metric);
        }

        public PlaneSearchParams withMetric(String metric) {
            return new PlaneSearchParams(plane, property, query, label, k, metric);
        }
    }

    public record PlaneQueryParams(
            String plane,
            Map<String, Object> plan,
            Long asOf,
            Long asOfMs) {

        public static PlaneQueryParams of(String plane, Map<String, Object> plan) {
            return new PlaneQueryParams(plane, plan, null, null);
        }

        public PlaneQueryParams withAsOf(Long asOf) {
            return new PlaneQueryParams(plane, plan, asOf, asOfMs);
        }

        public PlaneQueryParams withAsOfMs(Long asOfMs) {
            return new PlaneQueryParams(plane, plan, asOf, asOfMs);
        }
    }

    public record PlaneCypherParams(
            String plane,
            String query,
            String embed,
            Map<String, Object> params) {

        public static PlaneCypherParams of(String plane, String query) {
            return new PlaneCypherParams(plane, query, null, null);
        }

        public PlaneCypherParams withEmbed(String embed) {
            return new PlaneCypherParams(plane, query, embed, params);
        }

        public PlaneCypherParams withParams(Map<String, Object> params) {
            return new PlaneCypherParams(plane, query, embed, params);
        }
    }

    public record PlaneFindParams(
            String plane,
            String q,
            Long limit,
            Boolean semantic,
            String provider,
            String embedModel,
            Long asOf,
            Long asOfMs) {

        public static PlaneFindParams of(String plane, String q) {
            return new PlaneFindParams(plane, q, null, null, null, null, null, null);
        }

        public PlaneFindParams withLimit(Long limit) {
            return new PlaneFindParams(plane, q, limit, semantic, provider, embedModel, asOf, asOfMs);
        }

        public PlaneFindParams withSemantic(Boolean semantic) {
            return new PlaneFindParams(plane, q, limit, semantic, provider, embedModel, asOf, asOfMs);
        }

        public PlaneFindParams withProvider(String provider) {
            return new PlaneFindParams(plane, q, limit, semantic, provider, embedModel, asOf, asOfMs);
        }

        public PlaneFindParams withEmbedModel(String embedModel) {
            return new PlaneFindParams(plane, q, limit, semantic, provider, embedModel, asOf, asOfMs);
        }

        public PlaneFindParams withAsOf(Long asOf) {
            return new PlaneFindParams(plane, q, limit, semantic, provider, embedModel, asOf, asOfMs);
        }

        public PlaneFindParams withAsOfMs(Long asOfMs) {
            return new PlaneFindParams(plane, q, limit, semantic, provider, embedModel, asOf, asOfMs);
        }
    }

    public record PlaneAlgoParams(
            String plane,
            String algo,
            String label,
            Long limit,
            Double damping,
            Long maxIters,
            Double tolerance,
            Long src,
            Long dst,
            String dir,
            String weight,
            Long maxLevels,
            Double minGain) {

        public static PlaneAlgoParams of(String plane, String algo) {
            return new PlaneAlgoParams(plane, algo, null, null, null, null, null, null, null, null, null, null, null);
        }

        public PlaneAlgoParams withLabel(String label) {
            return new PlaneAlgoParams(plane, algo, label, limit, damping, maxIters, tolerance, src, dst, dir, weight, maxLevels, minGain);
        }

        public PlaneAlgoParams withLimit(Long limit) {
            return new PlaneAlgoParams(plane, algo, label, limit, damping, maxIters, tolerance, src, dst, dir, weight, maxLevels, minGain);
        }

        public PlaneAlgoParams withDamping(Double damping) {
            return new PlaneAlgoParams(plane, algo, label, limit, damping, maxIters, tolerance, src, dst, dir, weight, maxLevels, minGain);
        }

        public PlaneAlgoParams withMaxIters(Long maxIters) {
            return new PlaneAlgoParams(plane, algo, label, limit, damping, maxIters, tolerance, src, dst, dir, weight, maxLevels, minGain);
        }

        public PlaneAlgoParams withTolerance(Double tolerance) {
            return new PlaneAlgoParams(plane, algo, label, limit, damping, maxIters, tolerance, src, dst, dir, weight, maxLevels, minGain);
        }

        public PlaneAlgoParams withSrc(Long src) {
            return new PlaneAlgoParams(plane, algo, label, limit, damping, maxIters, tolerance, src, dst, dir, weight, maxLevels, minGain);
        }

        public PlaneAlgoParams withDst(Long dst) {
            return new PlaneAlgoParams(plane, algo, label, limit, damping, maxIters, tolerance, src, dst, dir, weight, maxLevels, minGain);
        }

        public PlaneAlgoParams withDir(String dir) {
            return new PlaneAlgoParams(plane, algo, label, limit, damping, maxIters, tolerance, src, dst, dir, weight, maxLevels, minGain);
        }

        public PlaneAlgoParams withWeight(String weight) {
            return new PlaneAlgoParams(plane, algo, label, limit, damping, maxIters, tolerance, src, dst, dir, weight, maxLevels, minGain);
        }

        public PlaneAlgoParams withMaxLevels(Long maxLevels) {
            return new PlaneAlgoParams(plane, algo, label, limit, damping, maxIters, tolerance, src, dst, dir, weight, maxLevels, minGain);
        }

        public PlaneAlgoParams withMinGain(Double minGain) {
            return new PlaneAlgoParams(plane, algo, label, limit, damping, maxIters, tolerance, src, dst, dir, weight, maxLevels, minGain);
        }
    }

    public record PlaneHybridParams(
            String plane,
            String q,
            String label,
            String vectorProp,
            String keywordProp,
            String metric,
            Long graphHops,
            Double graphDecay,
            Double wVector,
            Double wKeyword,
            Double wGraph,
            Long k,
            Long candidates,
            String provider,
            String embedModel) {

        public static PlaneHybridParams of(String plane, String q) {
            return new PlaneHybridParams(plane, q, null, null, null, null, null, null, null, null, null, null, null, null, null);
        }

        public PlaneHybridParams withLabel(String label) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withVectorProp(String vectorProp) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withKeywordProp(String keywordProp) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withMetric(String metric) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withGraphHops(Long graphHops) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withGraphDecay(Double graphDecay) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withWVector(Double wVector) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withWKeyword(Double wKeyword) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withWGraph(Double wGraph) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withK(Long k) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withCandidates(Long candidates) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withProvider(String provider) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }

        public PlaneHybridParams withEmbedModel(String embedModel) {
            return new PlaneHybridParams(plane, q, label, vectorProp, keywordProp, metric, graphHops, graphDecay, wVector, wKeyword, wGraph, k, candidates, provider, embedModel);
        }
    }

    public record PlaneAskParams(
            String plane,
            String question,
            Boolean dryRun,
            Long maxAttempts,
            Long limit,
            String provider,
            String model,
            String embedProvider,
            String embedModel) {

        public static PlaneAskParams of(String plane, String question) {
            return new PlaneAskParams(plane, question, null, null, null, null, null, null, null);
        }

        public PlaneAskParams withDryRun(Boolean dryRun) {
            return new PlaneAskParams(plane, question, dryRun, maxAttempts, limit, provider, model, embedProvider, embedModel);
        }

        public PlaneAskParams withMaxAttempts(Long maxAttempts) {
            return new PlaneAskParams(plane, question, dryRun, maxAttempts, limit, provider, model, embedProvider, embedModel);
        }

        public PlaneAskParams withLimit(Long limit) {
            return new PlaneAskParams(plane, question, dryRun, maxAttempts, limit, provider, model, embedProvider, embedModel);
        }

        public PlaneAskParams withProvider(String provider) {
            return new PlaneAskParams(plane, question, dryRun, maxAttempts, limit, provider, model, embedProvider, embedModel);
        }

        public PlaneAskParams withModel(String model) {
            return new PlaneAskParams(plane, question, dryRun, maxAttempts, limit, provider, model, embedProvider, embedModel);
        }

        public PlaneAskParams withEmbedProvider(String embedProvider) {
            return new PlaneAskParams(plane, question, dryRun, maxAttempts, limit, provider, model, embedProvider, embedModel);
        }

        public PlaneAskParams withEmbedModel(String embedModel) {
            return new PlaneAskParams(plane, question, dryRun, maxAttempts, limit, provider, model, embedProvider, embedModel);
        }
    }

    public record PlaneIndexesParams(
            String plane) {

        public static PlaneIndexesParams of(String plane) {
            return new PlaneIndexesParams(plane);
        }
    }

    public record IndexEnsureParams(
            String plane,
            String label,
            String property,
            String kind,
            String metric,
            String language) {

        public static IndexEnsureParams of(String plane, String label, String property) {
            return new IndexEnsureParams(plane, label, property, null, null, null);
        }

        public IndexEnsureParams withKind(String kind) {
            return new IndexEnsureParams(plane, label, property, kind, metric, language);
        }

        public IndexEnsureParams withMetric(String metric) {
            return new IndexEnsureParams(plane, label, property, kind, metric, language);
        }

        public IndexEnsureParams withLanguage(String language) {
            return new IndexEnsureParams(plane, label, property, kind, metric, language);
        }
    }

    public record GraphSeedParams(
            String plane,
            String label,
            Long limit,
            Long asOf,
            Long asOfMs) {

        public static GraphSeedParams of(String plane) {
            return new GraphSeedParams(plane, null, null, null, null);
        }

        public GraphSeedParams withLabel(String label) {
            return new GraphSeedParams(plane, label, limit, asOf, asOfMs);
        }

        public GraphSeedParams withLimit(Long limit) {
            return new GraphSeedParams(plane, label, limit, asOf, asOfMs);
        }

        public GraphSeedParams withAsOf(Long asOf) {
            return new GraphSeedParams(plane, label, limit, asOf, asOfMs);
        }

        public GraphSeedParams withAsOfMs(Long asOfMs) {
            return new GraphSeedParams(plane, label, limit, asOf, asOfMs);
        }
    }

    public record GraphExpandParams(
            String plane,
            long id,
            String direction,
            String type,
            Long limit,
            Long asOf,
            Long asOfMs) {

        public static GraphExpandParams of(String plane, long id) {
            return new GraphExpandParams(plane, id, null, null, null, null, null);
        }

        public GraphExpandParams withDirection(String direction) {
            return new GraphExpandParams(plane, id, direction, type, limit, asOf, asOfMs);
        }

        public GraphExpandParams withType(String type) {
            return new GraphExpandParams(plane, id, direction, type, limit, asOf, asOfMs);
        }

        public GraphExpandParams withLimit(Long limit) {
            return new GraphExpandParams(plane, id, direction, type, limit, asOf, asOfMs);
        }

        public GraphExpandParams withAsOf(Long asOf) {
            return new GraphExpandParams(plane, id, direction, type, limit, asOf, asOfMs);
        }

        public GraphExpandParams withAsOfMs(Long asOfMs) {
            return new GraphExpandParams(plane, id, direction, type, limit, asOf, asOfMs);
        }
    }

    public record DigestRunParams(
            String plane,
            String text,
            String chat,
            String embed,
            String model,
            String embedModel,
            String source,
            Boolean noEmbed,
            Boolean link,
            Long concurrency,
            Long chunkChars,
            String mode) {

        public static DigestRunParams of(String plane, String text) {
            return new DigestRunParams(plane, text, null, null, null, null, null, null, null, null, null, null);
        }

        public DigestRunParams withChat(String chat) {
            return new DigestRunParams(plane, text, chat, embed, model, embedModel, source, noEmbed, link, concurrency, chunkChars, mode);
        }

        public DigestRunParams withEmbed(String embed) {
            return new DigestRunParams(plane, text, chat, embed, model, embedModel, source, noEmbed, link, concurrency, chunkChars, mode);
        }

        public DigestRunParams withModel(String model) {
            return new DigestRunParams(plane, text, chat, embed, model, embedModel, source, noEmbed, link, concurrency, chunkChars, mode);
        }

        public DigestRunParams withEmbedModel(String embedModel) {
            return new DigestRunParams(plane, text, chat, embed, model, embedModel, source, noEmbed, link, concurrency, chunkChars, mode);
        }

        public DigestRunParams withSource(String source) {
            return new DigestRunParams(plane, text, chat, embed, model, embedModel, source, noEmbed, link, concurrency, chunkChars, mode);
        }

        public DigestRunParams withNoEmbed(Boolean noEmbed) {
            return new DigestRunParams(plane, text, chat, embed, model, embedModel, source, noEmbed, link, concurrency, chunkChars, mode);
        }

        public DigestRunParams withLink(Boolean link) {
            return new DigestRunParams(plane, text, chat, embed, model, embedModel, source, noEmbed, link, concurrency, chunkChars, mode);
        }

        public DigestRunParams withConcurrency(Long concurrency) {
            return new DigestRunParams(plane, text, chat, embed, model, embedModel, source, noEmbed, link, concurrency, chunkChars, mode);
        }

        public DigestRunParams withChunkChars(Long chunkChars) {
            return new DigestRunParams(plane, text, chat, embed, model, embedModel, source, noEmbed, link, concurrency, chunkChars, mode);
        }

        public DigestRunParams withMode(String mode) {
            return new DigestRunParams(plane, text, chat, embed, model, embedModel, source, noEmbed, link, concurrency, chunkChars, mode);
        }
    }

    public record DigestWriteResult(
            Long nodesWritten,
            Long edgesWritten) {
    }

    public record DigestWriteParams(
            String plane,
            List<Map<String, Object>> nodes,
            List<Map<String, Object>> edges) {

        public static DigestWriteParams of(String plane, List<Map<String, Object>> nodes) {
            return new DigestWriteParams(plane, nodes, null);
        }

        public DigestWriteParams withEdges(List<Map<String, Object>> edges) {
            return new DigestWriteParams(plane, nodes, edges);
        }
    }

    public record NodeCreateParams(
            String plane,
            String key,
            List<String> labels,
            Map<String, Object> properties) {

        public static NodeCreateParams of(String plane) {
            return new NodeCreateParams(plane, null, null, null);
        }

        public NodeCreateParams withKey(String key) {
            return new NodeCreateParams(plane, key, labels, properties);
        }

        public NodeCreateParams withLabels(List<String> labels) {
            return new NodeCreateParams(plane, key, labels, properties);
        }

        public NodeCreateParams withProperties(Map<String, Object> properties) {
            return new NodeCreateParams(plane, key, labels, properties);
        }
    }

    public record NodeUpdateParams(
            String plane,
            Long id,
            String key,
            Map<String, Object> set,
            List<String> unset,
            List<String> labels) {

        public static NodeUpdateParams of(String plane) {
            return new NodeUpdateParams(plane, null, null, null, null, null);
        }

        public NodeUpdateParams withId(Long id) {
            return new NodeUpdateParams(plane, id, key, set, unset, labels);
        }

        public NodeUpdateParams withKey(String key) {
            return new NodeUpdateParams(plane, id, key, set, unset, labels);
        }

        public NodeUpdateParams withSet(Map<String, Object> set) {
            return new NodeUpdateParams(plane, id, key, set, unset, labels);
        }

        public NodeUpdateParams withUnset(List<String> unset) {
            return new NodeUpdateParams(plane, id, key, set, unset, labels);
        }

        public NodeUpdateParams withLabels(List<String> labels) {
            return new NodeUpdateParams(plane, id, key, set, unset, labels);
        }
    }

    public record NodeDeleteParams(
            String plane,
            Long id,
            String key) {

        public static NodeDeleteParams of(String plane) {
            return new NodeDeleteParams(plane, null, null);
        }

        public NodeDeleteParams withId(Long id) {
            return new NodeDeleteParams(plane, id, key);
        }

        public NodeDeleteParams withKey(String key) {
            return new NodeDeleteParams(plane, id, key);
        }
    }

    public record EdgeCreateParams(
            String plane,
            Object src,
            Object dst,
            String type,
            Map<String, Object> properties) {

        public static EdgeCreateParams of(String plane, Object src, Object dst, String type) {
            return new EdgeCreateParams(plane, src, dst, type, null);
        }

        public EdgeCreateParams withProperties(Map<String, Object> properties) {
            return new EdgeCreateParams(plane, src, dst, type, properties);
        }
    }

    public record EdgeUpdateParams(
            String plane,
            long edge,
            Map<String, Object> set,
            List<String> unset,
            String type) {

        public static EdgeUpdateParams of(String plane, long edge) {
            return new EdgeUpdateParams(plane, edge, null, null, null);
        }

        public EdgeUpdateParams withSet(Map<String, Object> set) {
            return new EdgeUpdateParams(plane, edge, set, unset, type);
        }

        public EdgeUpdateParams withUnset(List<String> unset) {
            return new EdgeUpdateParams(plane, edge, set, unset, type);
        }

        public EdgeUpdateParams withType(String type) {
            return new EdgeUpdateParams(plane, edge, set, unset, type);
        }
    }

    public record EdgeDeleteParams(
            String plane,
            long edge) {

        public static EdgeDeleteParams of(String plane, long edge) {
            return new EdgeDeleteParams(plane, edge);
        }
    }

    public record PlaneCreateParams(
            String name,
            Map<String, Object> properties) {

        public static PlaneCreateParams of(String name) {
            return new PlaneCreateParams(name, null);
        }

        public PlaneCreateParams withProperties(Map<String, Object> properties) {
            return new PlaneCreateParams(name, properties);
        }
    }

    public record PlaneRenameParams(
            String plane,
            String to) {

        public static PlaneRenameParams of(String plane, String to) {
            return new PlaneRenameParams(plane, to);
        }
    }

    public record PlaneSetPropsParams(
            String plane,
            Map<String, Object> properties) {

        public static PlaneSetPropsParams of(String plane, Map<String, Object> properties) {
            return new PlaneSetPropsParams(plane, properties);
        }
    }

    public record PlaneDeleteParams(
            String plane) {

        public static PlaneDeleteParams of(String plane) {
            return new PlaneDeleteParams(plane);
        }
    }

    /** This OpenRPC service description. (access: read) */
    public Map<String, Object> rpcDiscover() throws DrsgException {
        return call("rpc.discover", null, new TypeReference<Map<String, Object>>() {});
    }

    /** Plane/node/edge counts plus the on-disk file size when persistent. (access: read) */
    public DbStats dbStats() throws DrsgException {
        return call("db.stats", null, new TypeReference<DbStats>() {});
    }

    /** The soft-schema catalog rolled up across every plane. (access: read) */
    public Map<String, Object> dbCatalog() throws DrsgException {
        return call("db.catalog", null, new TypeReference<Map<String, Object>>() {});
    }

    /** Every plane with its id, name, counts, and own properties. (access: read) */
    public List<PlaneCard> planeList() throws DrsgException {
        return call("plane.list", null, new TypeReference<List<PlaneCard>>() {});
    }

    /** One plane's soft schema (labels, property descriptions, edge types, counts). (access: read) */
    public Map<String, Object> planeCatalog(PlaneCatalogParams params) throws DrsgException {
        return call("plane.catalog", params, new TypeReference<Map<String, Object>>() {});
    }

    /** One node by id or external key; null if absent. (access: read) */
    public NodeRecord nodeGet(NodeGetParams params) throws DrsgException {
        return call("node.get", params, new TypeReference<NodeRecord>() {});
    }

    /** 1-hop expansion as {node, edge} id pairs. (access: read) */
    public List<PlaneNeighborsItem> planeNeighbors(PlaneNeighborsParams params) throws DrsgException {
        return call("plane.neighbors", params, new TypeReference<List<PlaneNeighborsItem>>() {});
    }

    /** Time-travel window: oldest and latest commit sequences a read can be pinned to (native backend only). (access: read) */
    public PlaneHistoryResult planeHistory() throws DrsgException {
        return call("plane.history", null, new TypeReference<PlaneHistoryResult>() {});
    }

    /** Vector top-k over a property; returns scored node records. (access: read) */
    public List<NodeRecord> planeSearch(PlaneSearchParams params) throws DrsgException {
        return call("plane.search", params, new TypeReference<List<NodeRecord>>() {});
    }

    /** Run a serialized logical plan verbatim; returns scored rows. (access: read) */
    public List<NodeRecord> planeQuery(PlaneQueryParams params) throws DrsgException {
        return call("plane.query", params, new TypeReference<List<NodeRecord>>() {});
    }

    /** Run a statement in the query language (openCypher subset). A read returns {nodes, edges, count}; a write (CREATE/MERGE/SET/REMOVE/DELETE) returns {write: true, ...change-counts}. Write-gated. (access: write) */
    public Map<String, Object> planeCypher(PlaneCypherParams params) throws DrsgException {
        return call("plane.cypher", params, new TypeReference<Map<String, Object>>() {});
    }

    /** Text (or semantic) search over the plane's nodes and edges. (access: read) */
    public FindResult planeFind(PlaneFindParams params) throws DrsgException {
        return call("plane.find", params, new TypeReference<FindResult>() {});
    }

    /** Run a graph algorithm (pagerank | components | shortest_path | louvain) over the plane or one label subset, read-only over a single snapshot. (access: read) */
    public Map<String, Object> planeAlgo(PlaneAlgoParams params) throws DrsgException {
        return call("plane.algo", params, new TypeReference<Map<String, Object>>() {});
    }

    /** Hybrid retrieval: fuse vector similarity, BM25 keyword, and graph-proximity channels into one ranking. Enable a channel by naming its property (vector_prop/keyword_prop) or setting graph_hops; the vector channel embeds q server-side. (access: read) */
    public Map<String, Object> planeHybrid(PlaneHybridParams params) throws DrsgException {
        return call("plane.hybrid", params, new TypeReference<Map<String, Object>>() {});
    }

    /** Natural-language query: an LLM turns the question into a read-only LogicalPlan, runs it (unless dry_run), and returns the generated plan plus result node records. With embed_provider, the model can call find_edge/find_entity embedding tools to ground the plan. Keys from the server env. (access: read) */
    public Map<String, Object> planeAsk(PlaneAskParams params) throws DrsgException {
        return call("plane.ask", params, new TypeReference<Map<String, Object>>() {});
    }

    /** The search indexes declared on a plane (vector + keyword), so a client can offer only the channels that actually exist. (access: read) */
    public Map<String, Object> planeIndexes(PlaneIndexesParams params) throws DrsgException {
        return call("plane.indexes", params, new TypeReference<Map<String, Object>>() {});
    }

    /** Declare (and build) a search index on (label, property): a keyword (BM25) or vector (embedding) index. Idempotent. (access: admin) */
    public Map<String, Object> indexEnsure(IndexEnsureParams params) throws DrsgException {
        return call("index.ensure", params, new TypeReference<Map<String, Object>>() {});
    }

    /** An initial canvas: up to `limit` nodes plus the edges induced among them. (access: read) */
    public Subgraph graphSeed(GraphSeedParams params) throws DrsgException {
        return call("graph.seed", params, new TypeReference<Subgraph>() {});
    }

    /** Hub-safe 1-hop neighbourhood around a node: neighbour + connecting-edge records. (access: read) */
    public Subgraph graphExpand(GraphExpandParams params) throws DrsgException {
        return call("graph.expand", params, new TypeReference<Subgraph>() {});
    }

    /** Extract a node/edge proposal from text via the LLM (dry-run; spends provider credits). (access: write) */
    public Map<String, Object> digestRun(DigestRunParams params) throws DrsgException {
        return call("digest.run", params, new TypeReference<Map<String, Object>>() {});
    }

    /** Write a previously-computed proposal into the plane via the bulk path (no LLM call). (access: write) */
    public DigestWriteResult digestWrite(DigestWriteParams params) throws DrsgException {
        return call("digest.write", params, new TypeReference<DigestWriteResult>() {});
    }

    /** Add a node with an optional stable external key and labels. (access: write) */
    public NodeRecord nodeCreate(NodeCreateParams params) throws DrsgException {
        return call("node.create", params, new TypeReference<NodeRecord>() {});
    }

    /** Patch a node: `set`/`unset` its properties, and `labels` (when present) replaces its label set. (access: write) */
    public NodeRecord nodeUpdate(NodeUpdateParams params) throws DrsgException {
        return call("node.update", params, new TypeReference<NodeRecord>() {});
    }

    /** Delete a node and cascade to its incident edges. (access: write) */
    public Deleted nodeDelete(NodeDeleteParams params) throws DrsgException {
        return call("node.delete", params, new TypeReference<Deleted>() {});
    }

    /** Add a directed edge between two existing nodes (each named by id or key). (access: write) */
    public EdgeRecord edgeCreate(EdgeCreateParams params) throws DrsgException {
        return call("edge.create", params, new TypeReference<EdgeRecord>() {});
    }

    /** Patch an edge: `set`/`unset` its properties, and `type` (when present) changes its type. (access: write) */
    public EdgeRecord edgeUpdate(EdgeUpdateParams params) throws DrsgException {
        return call("edge.update", params, new TypeReference<EdgeRecord>() {});
    }

    /** Delete one edge. (access: write) */
    public Deleted edgeDelete(EdgeDeleteParams params) throws DrsgException {
        return call("edge.delete", params, new TypeReference<Deleted>() {});
    }

    /** Make a new, empty plane. (access: admin) */
    public PlaneRef planeCreate(PlaneCreateParams params) throws DrsgException {
        return call("plane.create", params, new TypeReference<PlaneRef>() {});
    }

    /** Rename an existing plane. (access: admin) */
    public PlaneRef planeRename(PlaneRenameParams params) throws DrsgException {
        return call("plane.rename", params, new TypeReference<PlaneRef>() {});
    }

    /** Replace a plane's own property map. (access: admin) */
    public Map<String, Object> planeSetProps(PlaneSetPropsParams params) throws DrsgException {
        return call("plane.set_props", params, new TypeReference<Map<String, Object>>() {});
    }

    /** Drop a plane and everything on it (the startup plane cannot be dropped). (access: admin) */
    public Deleted planeDelete(PlaneDeleteParams params) throws DrsgException {
        return call("plane.delete", params, new TypeReference<Deleted>() {});
    }

}
