// pgjdbc smoke script for latticed, text mode (issue 607).
//
// Runs against the server named by LATTICE_DSN (a postgresql:// URL) and prints one JSON object per step.
// tests/drivers/smoke.sh compares the lines with expected.jsonl and the pins in known.jsonl.
//
// latticed refuses binary format codes with 0A000 (#608). pgjdbc sends int, long and boolean setters in
// binary, so the steps bind every value with setString on a connection with stringtype=unspecified (the
// parameter type is left to the server, as node-postgres does) and binaryTransfer=false (text results).
// The last step uses setInt, the way most JDBC code binds an int. With pgjdbc 42.7.14 it matched
// PostgreSQL's output against latticed, so it has no known-failure pin (not checked whether pgjdbc sent it as text).
import java.net.URI;
import java.sql.Connection;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Statement;
import java.sql.Types;
import java.util.ArrayList;
import java.util.List;

public final class Smoke {
    private static final String INSERT = "INSERT INTO drv_smoke VALUES (?, ?, ?, ?)";

    private interface Body {
        String run() throws SQLException;
    }

    private static String json(String s) {
        StringBuilder b = new StringBuilder("\"");
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '"' || c == '\\') {
                b.append('\\').append(c);
            } else if (c < 0x20) {
                b.append(String.format("\\u%04x", (int) c));
            } else {
                b.append(c);
            }
        }
        return b.append('"').toString();
    }

    private static String value(Object o) {
        if (o == null) {
            return "null";
        }
        if (o instanceof Number || o instanceof Boolean) {
            return o.toString();
        }
        return json(o.toString());
    }

    // The record's body, without the step name and tags: a fragment such as `"rowcount":1`.
    private static void step(String name, String[] tags, Body body) {
        String fragment;
        try {
            fragment = body.run();
        } catch (SQLException e) {
            String state = e.getSQLState();
            if (state != null && !state.isEmpty()) {
                fragment = "\"sqlstate\":" + json(state);
            } else {
                String message = String.valueOf(e.getMessage());
                int nl = message.indexOf('\n');
                fragment = "\"client_error\":" + json(nl >= 0 ? message.substring(0, nl) : message);
            }
        }
        StringBuilder tagList = new StringBuilder("[");
        for (int i = 0; i < tags.length; i++) {
            tagList.append(i == 0 ? "" : ",").append(json(tags[i]));
        }
        tagList.append(']');
        System.out.println("{\"step\":" + json(name) + ",\"tags\":" + tagList
            + (fragment.isEmpty() ? "" : "," + fragment) + "}");
    }

    private static void bind(PreparedStatement ps, Object... values) throws SQLException {
        for (int i = 0; i < values.length; i++) {
            if (values[i] == null) {
                ps.setNull(i + 1, Types.NULL);
            } else {
                ps.setString(i + 1, values[i].toString());
            }
        }
    }

    private static String rows(ResultSet rs) throws SQLException {
        int columns = rs.getMetaData().getColumnCount();
        List<String> out = new ArrayList<>();
        while (rs.next()) {
            List<String> row = new ArrayList<>();
            for (int c = 1; c <= columns; c++) {
                row.add(value(rs.getObject(c)));
            }
            out.add("[" + String.join(",", row) + "]");
        }
        return "\"rows\":[" + String.join(",", out) + "]";
    }

    private static String query(Connection conn, String sql, Object... values) throws SQLException {
        try (PreparedStatement ps = conn.prepareStatement(sql)) {
            bind(ps, values);
            try (ResultSet rs = ps.executeQuery()) {
                return rows(rs);
            }
        }
    }

    private static String count(Connection conn, String sql, Object... values) throws SQLException {
        try (PreparedStatement ps = conn.prepareStatement(sql)) {
            bind(ps, values);
            return "\"rowcount\":" + ps.executeUpdate();
        }
    }

    private static String ok(Connection conn, String sql) throws SQLException {
        try (Statement st = conn.createStatement()) {
            st.execute(sql);
            return "\"ok\":true";
        }
    }

    private static Connection connect(String dsn) throws SQLException {
        URI uri = URI.create(dsn);
        if (!"postgresql".equals(uri.getScheme()) || uri.getUserInfo() == null || uri.getHost() == null
            || uri.getPort() < 0) {
            throw new IllegalArgumentException("LATTICE_DSN must look like postgresql://USER@HOST:PORT/DATABASE");
        }
        if (uri.getQuery() == null || !uri.getQuery().contains("sslmode=disable")) {
            throw new IllegalArgumentException("LATTICE_DSN must set sslmode=disable: this script runs over loopback without TLS");
        }
        String url = "jdbc:postgresql://" + uri.getHost() + ":" + uri.getPort() + uri.getPath()
            + "?sslmode=disable&user=" + uri.getUserInfo() + "&stringtype=unspecified&binaryTransfer=false"
            + "&loginTimeout=20&socketTimeout=30";
        Connection conn = java.sql.DriverManager.getConnection(url);
        conn.setAutoCommit(true);
        return conn;
    }

    public static void main(String[] args) throws Exception {
        String dsn = System.getenv("LATTICE_DSN");
        if (dsn == null || dsn.isEmpty()) {
            System.err.println("LATTICE_DSN is not set");
            System.exit(2);
        }
        Connection conn = connect(dsn);

        step("select_literals", new String[] {"select", "int", "text", "bool"},
            () -> query(conn, "SELECT 1 AS one, 'x' AS s, true AS b"));
        step("create_table", new String[] {"ddl"},
            () -> ok(conn, "CREATE TABLE drv_smoke (id INT PRIMARY KEY, name TEXT NOT NULL, qty BIGINT, ok BOOLEAN)"));
        step("insert_bolt", new String[] {"dml", "parameter", "int", "text", "bool", "bigint"},
            () -> count(conn, INSERT, 1, "bolt", 9000000000L, true));
        step("insert_nut_null", new String[] {"dml", "parameter", "int", "text", "bool", "null"},
            () -> count(conn, INSERT, 2, "nut", null, false));
        step("select_with_params", new String[] {"select", "parameter", "int", "text", "bool", "bigint", "null"},
            () -> query(conn, "SELECT id, name, qty, ok FROM drv_smoke WHERE id = ? OR name = ? ORDER BY id", 2, "bolt"));
        step("update_with_params", new String[] {"dml", "parameter", "bigint"},
            () -> count(conn, "UPDATE drv_smoke SET qty = ? WHERE id = ?", 7, 1));
        step("delete_with_params", new String[] {"dml", "parameter"},
            () -> count(conn, "DELETE FROM drv_smoke WHERE id = ?", 2));
        step("transaction_commit", new String[] {"transaction", "dml", "parameter"}, () -> {
            conn.setAutoCommit(false);
            try {
                String r = count(conn, INSERT, 3, "pin", 4, true);
                conn.commit();
                return r;
            } catch (SQLException e) {
                conn.rollback();
                throw e;
            } finally {
                conn.setAutoCommit(true);
            }
        });
        step("select_after_commit", new String[] {"select"},
            () -> query(conn, "SELECT id FROM drv_smoke ORDER BY id"));
        step("transaction_rollback", new String[] {"transaction", "dml", "parameter"}, () -> {
            conn.setAutoCommit(false);
            try {
                return count(conn, INSERT, 4, "gone", 0, true);
            } finally {
                conn.rollback();
                conn.setAutoCommit(true);
            }
        });
        step("select_after_rollback", new String[] {"select", "parameter"},
            () -> query(conn, "SELECT id FROM drv_smoke WHERE id = ?", 4));
        step("error_duplicate_key", new String[] {"error", "dml", "parameter", "null"},
            () -> count(conn, INSERT, 1, "dup", null, null));
        step("error_syntax", new String[] {"error"}, () -> ok(conn, "SELECT FROM FROM drv_smoke"));
        step("select_after_errors", new String[] {"select"},
            () -> query(conn, "SELECT id FROM drv_smoke ORDER BY id"));
        step("drop_table", new String[] {"ddl"}, () -> ok(conn, "DROP TABLE drv_smoke"));
        step("known_setint_select", new String[] {"select", "parameter", "int"}, () -> {
            try (PreparedStatement ps = conn.prepareStatement("SELECT ?::int AS n")) {
                ps.setInt(1, 7);
                try (ResultSet rs = ps.executeQuery()) {
                    return rows(rs);
                }
            }
        });

        conn.close();
        System.out.println("{\"step\":\"close\",\"tags\":[\"close\"],\"ok\":true}");
    }
}
