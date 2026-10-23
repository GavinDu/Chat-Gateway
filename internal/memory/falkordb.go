package memory

import (
	"context"
	"fmt"
	"strconv"
	"strings"
	"time"

	"github.com/gomodule/redigo/redis"
	"github.com/rs/zerolog/log"

	"go-proxy/internal/types"
)

// FalkorDBConfig holds the connection parameters for a FalkorDB instance.
// FalkorDB exposes a Redis-compatible protocol, so the standard Redis port
// (6379) and AUTH mechanism are used.
type FalkorDBConfig struct {
	Host     string
	Port     int
	Password string
	Graph    string
}

// FalkorStore persists conversations in a FalkorDB property graph using the
// GRAPH.QUERY Redis command directly (no third-party wrapper needed beyond
// the redigo Redis client that is already a transitive dependency).
//
// Graph schema
//
//	(:Conversation {id, created_at, updated_at})
//	    -[:HAS_MESSAGE {seq}]->
//	(:Message {conv_id, seq, role, content, provider, model, created_at})
//
// Messages are ordered by the seq integer; ORDER BY seq returns chronological
// history regardless of insertion order.
type FalkorStore struct {
	pool      *redis.Pool
	graphName string
}

// NewFalkorStore creates a FalkorStore, verifies connectivity, and initialises
// graph indexes.  Returns an error if FalkorDB is unreachable.
func NewFalkorStore(cfg FalkorDBConfig) (*FalkorStore, error) {
	addr := fmt.Sprintf("%s:%d", cfg.Host, cfg.Port)

	pool := &redis.Pool{
		MaxIdle:     5,
		IdleTimeout: 4 * time.Minute,
		Dial: func() (redis.Conn, error) {
			c, err := redis.Dial("tcp", addr,
				redis.DialConnectTimeout(5*time.Second),
				redis.DialReadTimeout(30*time.Second),
				redis.DialWriteTimeout(30*time.Second),
			)
			if err != nil {
				return nil, err
			}
			if cfg.Password != "" {
				if _, err2 := c.Do("AUTH", cfg.Password); err2 != nil {
					_ = c.Close()
					return nil, err2
				}
			}
			return c, nil
		},
	}

	// Verify connectivity before accepting the connection.
	conn := pool.Get()
	_, pingErr := conn.Do("PING")
	_ = conn.Close()
	if pingErr != nil {
		_ = pool.Close()
		return nil, fmt.Errorf("falkordb: ping %s: %w", addr, pingErr)
	}

	s := &FalkorStore{pool: pool, graphName: cfg.Graph}
	s.initIndexes()
	return s, nil
}

// initIndexes creates graph indexes to speed up lookups by conversation ID.
// Errors (e.g. index already exists) are logged at DEBUG level and ignored.
func (s *FalkorStore) initIndexes() {
	for _, q := range []string{
		`CREATE INDEX FOR (c:Conversation) ON (c.id)`,
		`CREATE INDEX FOR (m:Message) ON (m.conv_id)`,
	} {
		if _, err := s.execQuery(q); err != nil {
			log.Debug().Err(err).Str("query", q[:30]).Msg("memory: index init (may already exist)")
		}
	}
}

// execQuery runs q as a GRAPH.QUERY command and returns the results as a 2-D
// string slice.  Results[0] is the header row (column names from the RETURN
// clause).  Results[1:] are data rows.  Returns nil for write-only queries.
//
// FalkorDB uses a RedisGraph-compatible wire format:
//
//	GRAPH.QUERY <graphName> <cypherQuery>
//	  → [result_rows_array, statistics_array]
//
// result_rows_array[0] = column-name strings
// result_rows_array[1..] = data rows (each row = array of scalar values)
func (s *FalkorStore) execQuery(q string) ([][]string, error) {
	conn := s.pool.Get()
	defer func() { _ = conn.Close() }()

	r, err := redis.Values(conn.Do("GRAPH.QUERY", s.graphName, q))
	if err != nil {
		return nil, err
	}
	if len(r) < 1 {
		return nil, nil
	}

	rawRows, err := redis.Values(r[0], nil)
	if err != nil || len(rawRows) == 0 {
		return nil, err
	}

	results := make([][]string, 0, len(rawRows))
	for _, rawRow := range rawRows {
		// Each rawRow is itself a Redis array ([]interface{}).
		rowVals, vErr := redis.Values(rawRow, nil)
		if vErr != nil {
			// Scalar at row level (unusual, but handle gracefully).
			results = append(results, []string{anyToString(rawRow)})
			continue
		}
		cols := make([]string, 0, len(rowVals))
		for _, v := range rowVals {
			cols = append(cols, anyToString(v))
		}
		results = append(results, cols)
	}
	return results, nil
}

// anyToString converts a redigo-returned interface{} value to a string.
// FalkorDB scalar properties arrive as []byte (strings) or int64 (integers).
func anyToString(v interface{}) string {
	switch val := v.(type) {
	case []byte:
		return string(val)
	case string:
		return val
	case int64:
		return strconv.FormatInt(val, 10)
	case float64:
		return strconv.FormatFloat(val, 'f', -1, 64)
	case nil:
		return ""
	default:
		return fmt.Sprintf("%v", val)
	}
}

// esc escapes a string for embedding inside a Cypher double-quoted string
// literal, guarding against injection via content, roles, or IDs.
func esc(s string) string {
	var b strings.Builder
	b.Grow(len(s) + 4)
	for _, r := range s {
		switch r {
		case '\\':
			b.WriteString(`\\`)
		case '"':
			b.WriteString(`\"`)
		case '\n':
			b.WriteString(`\n`)
		case '\r':
			b.WriteString(`\r`)
		case '\t':
			b.WriteString(`\t`)
		default:
			b.WriteRune(r)
		}
	}
	return b.String()
}

// GetHistory returns all messages for conversationID in chronological order.
// Returns nil, nil when the conversation is unknown or has no messages.
func (s *FalkorStore) GetHistory(_ context.Context, conversationID string) ([]types.Message, error) {
	q := fmt.Sprintf(
		`MATCH (c:Conversation {id: "%s"})-[:HAS_MESSAGE]->(m:Message) RETURN m.role, m.content ORDER BY m.seq`,
		esc(conversationID),
	)
	results, err := s.execQuery(q)
	if err != nil {
		return nil, fmt.Errorf("falkordb: get history: %w", err)
	}
	// results[0] = header row, results[1:] = data
	if len(results) <= 1 {
		return nil, nil
	}
	msgs := make([]types.Message, 0, len(results)-1)
	for _, row := range results[1:] {
		if len(row) < 2 {
			continue
		}
		msgs = append(msgs, types.Message{Role: row[0], Content: row[1]})
	}
	return msgs, nil
}

// SaveTurn appends messages to the conversation graph, assigning seq numbers
// starting after the current last message.  Creates the Conversation node if
// it does not already exist.
func (s *FalkorStore) SaveTurn(_ context.Context, conversationID, provider, model string, messages []types.Message) error {
	if len(messages) == 0 {
		return nil
	}

	now := time.Now().Unix()
	convID := esc(conversationID)

	// 1. Upsert Conversation node.
	upsertQ := fmt.Sprintf(
		`MERGE (c:Conversation {id: "%s"}) ON CREATE SET c.created_at = %d, c.updated_at = %d ON MATCH SET c.updated_at = %d`,
		convID, now, now, now,
	)
	if _, err := s.execQuery(upsertQ); err != nil {
		return fmt.Errorf("falkordb: upsert conversation: %w", err)
	}

	// 2. Count existing messages to determine the starting seq offset.
	countQ := fmt.Sprintf(
		`MATCH (c:Conversation {id: "%s"})-[:HAS_MESSAGE]->(m:Message) RETURN count(m)`,
		convID,
	)
	countResult, err := s.execQuery(countQ)
	if err != nil {
		return fmt.Errorf("falkordb: count messages: %w", err)
	}
	startSeq := 0
	if len(countResult) > 1 && len(countResult[1]) > 0 {
		f, _ := strconv.ParseFloat(countResult[1][0], 64)
		startSeq = int(f)
	}

	// 3. Insert each message as a Message node linked to the Conversation.
	for i, msg := range messages {
		seq := startSeq + i
		msgProvider := ""
		msgModel := ""
		if msg.Role == "assistant" {
			msgProvider = provider
			msgModel = model
		}
		addQ := fmt.Sprintf(
			`MATCH (c:Conversation {id: "%s"}) `+
				`CREATE (m:Message {conv_id: "%s", seq: %d, role: "%s", content: "%s", provider: "%s", model: "%s", created_at: %d}) `+
				`CREATE (c)-[:HAS_MESSAGE]->(m)`,
			convID, convID, seq,
			esc(msg.Role), esc(msg.Content),
			esc(msgProvider), esc(msgModel),
			now,
		)
		if _, err2 := s.execQuery(addQ); err2 != nil {
			return fmt.Errorf("falkordb: add message seq %d: %w", seq, err2)
		}
	}

	log.Debug().
		Str("conversation_id", conversationID).
		Int("messages_added", len(messages)).
		Int("start_seq", startSeq).
		Str("provider", provider).
		Msg("memory: turn saved to FalkorDB")

	return nil
}

// Close releases the underlying Redis connection pool.
func (s *FalkorStore) Close() error {
	return s.pool.Close()
}
